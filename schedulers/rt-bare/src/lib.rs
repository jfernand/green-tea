#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use green_tea_arch::{Context, swap_context};
use green_tea_bare::platform::TimerInterrupt;
use green_tea_bare::stack::Stack;
use green_tea_bare::{Global, GlobalPtr};
use green_tea_rt::{Error, MutexId, Scheduler, TaskId, TaskState};

pub const MAX_TASKS: usize = 16;
pub const PRIORITIES: usize = 8;
pub const MAX_MUTEXES: usize = 8;

pub type TaskEntry = fn();

struct Slot {
    context: Context,
    stack: Option<Stack>,
    entry: Option<TaskEntry>,
    pending_reap: bool,
}

impl Slot {
    fn new() -> Self {
        Self {
            context: Context::default(),
            stack: None,
            entry: None,
            pending_reap: false,
        }
    }
}

struct Runtime {
    policy: Scheduler<MAX_TASKS, PRIORITIES, MAX_MUTEXES>,
    slots: [Slot; MAX_TASKS],
    boot: Context,
    current: Option<TaskId>,
    slice_ticks: u64,
}

impl Runtime {
    fn new(slice_ticks: u64) -> Self {
        Self {
            policy: Scheduler::new().expect("RT policy capacities are valid"),
            slots: core::array::from_fn(|_| Slot::new()),
            boot: Context::default(),
            current: None,
            slice_ticks,
        }
    }
}

static RUNTIME: Global<GlobalPtr<Runtime>> = Global::new(GlobalPtr::null());
static PREEMPTIONS: AtomicUsize = AtomicUsize::new(0);
static MAX_INTERRUPTS_OFF_TICKS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct InterruptSpan {
    was_on: bool,
    started_at: u64,
}

fn disable_interrupts<B: TimerInterrupt>() -> InterruptSpan {
    let was_on = B::interrupts_off();
    InterruptSpan {
        was_on,
        started_at: B::ticks(),
    }
}

fn restore_interrupts<B: TimerInterrupt>(span: InterruptSpan) {
    record_interrupts_off(B::ticks().saturating_sub(span.started_at));
    B::restore_interrupts(span.was_on);
}

fn runtime_ptr() -> *mut Runtime {
    let pointer = RUNTIME
        .get()
        .as_ptr();
    assert!(
        !pointer.is_null(),
        "the RT runtime has not been initialized"
    );
    pointer
}

fn with_runtime<B: TimerInterrupt, R>(f: impl FnOnce(&mut Runtime) -> R) -> R {
    B::without_interrupts(|| {
        assert_eq!(B::cpu(), 0, "the RT runtime is boot-CPU-only");
        let start = B::ticks();
        let pointer = runtime_ptr();
        // SAFETY: the RT runtime is boot-CPU-only, and every access is serialized with
        // interrupts masked. No reference escapes this closure.
        let result = f(unsafe { &mut *pointer });
        record_interrupts_off(B::ticks().saturating_sub(start));
        result
    })
}

fn record_interrupts_off(ticks: u64) {
    MAX_INTERRUPTS_OFF_TICKS.fetch_max(ticks, Ordering::Relaxed);
}

fn reap_pending<B: TimerInterrupt>() {
    with_runtime::<B, _>(|runtime| {
        for slot in &mut runtime.slots {
            if slot.pending_reap {
                slot.entry = None;
                slot.context = Context::default();
                slot.stack
                    .take();
                slot.pending_reap = false;
            }
        }
    });
}

/// Switch to the highest-priority ready task, or back to the boot context if none is ready.
/// The caller enters with interrupts disabled. `restore_interrupts` is the state saved by
/// the task operation that initiated this switch; interrupt handlers pass `false`.
fn switch_to_next<B: TimerInterrupt>(span: InterruptSpan) -> bool {
    let (from, to, same, deadline) = with_runtime::<B, _>(|runtime| {
        let old = runtime.current;
        let next = match runtime
            .policy
            .next_ready()
        {
            Ok(id) => Some(id),
            Err(Error::NoReadyTask) => None,
            Err(error) => panic!("RT scheduler selection failed: {error:?}"),
        };
        runtime.current = next;
        let from = if let Some(id) = old {
            &mut runtime.slots[id.index()].context as *mut Context
        } else {
            &mut runtime.boot as *mut Context
        };
        let to = if let Some(id) = next {
            &runtime.slots[id.index()].context as *const Context
        } else {
            &runtime.boot as *const Context
        };
        let same = from.cast_const() == to;
        let slice = if next.is_some() {
            B::ticks().saturating_add(runtime.slice_ticks)
        } else {
            u64::MAX
        };
        let deadline = runtime
            .policy
            .next_deadline()
            .unwrap_or(u64::MAX)
            .min(slice);
        (from, to, same, deadline)
    });
    B::set_deadline(deadline);
    record_interrupts_off(B::ticks().saturating_sub(span.started_at));
    let switched = !same;
    if switched {
        // SAFETY: task contexts and their stacks remain pinned in the global runtime; all
        // scheduling state is stable until the switch returns to this context.
        unsafe { swap_context(from, to) };
        let resumed_at = B::ticks();
        reap_pending::<B>();
        record_interrupts_off(B::ticks().saturating_sub(resumed_at));
    } else {
        reap_pending::<B>();
    }
    B::restore_interrupts(span.was_on);
    switched
}

fn tick<B: TimerInterrupt>() {
    let span = disable_interrupts::<B>();
    with_runtime::<B, _>(|runtime| {
        runtime
            .policy
            .wake_expired(B::ticks());
        if let Some(current) = runtime.current {
            if runtime
                .policy
                .state(current)
                == Ok(TaskState::Running)
            {
                runtime
                    .policy
                    .yield_task(current)
                    .expect("timer preempts a running task");
            }
        }
    });
    if switch_to_next::<B>(span) {
        PREEMPTIONS.fetch_add(1, Ordering::Relaxed);
    }
}

extern "C" fn task_entry<B: TimerInterrupt>(arg: *mut ()) -> ! {
    let id = TaskId::from_index(arg as usize).expect("RT task id fits its fixed pool");
    reap_pending::<B>();
    let entry = with_runtime::<B, _>(|runtime| {
        assert_eq!(runtime.current, Some(id), "RT task entered before dispatch");
        runtime.slots[id.index()]
            .entry
            .expect("dispatched RT task has an entry")
    });
    B::interrupts_on();
    entry();
    let span = disable_interrupts::<B>();
    with_runtime::<B, _>(|runtime| {
        runtime
            .policy
            .finish(id)
            .expect("completed RT task is running and owns no mutex");
        runtime.slots[id.index()].pending_reap = true;
    });
    switch_to_next::<B>(span);
    panic!("a completed RT task resumed");
}

/// Run a fixed-capacity set of function tasks with timer preemption and direct context
/// switching. The runtime starts with one main task at `main_priority`.
pub fn run<B: TimerInterrupt>(main: TaskEntry, main_priority: u8, slice_ms: u64) {
    assert_eq!(B::cpu(), 0, "the RT runtime is boot-CPU-only");
    let interrupts_were_on = B::interrupts_off();
    assert!(
        RUNTIME
            .get()
            .is_null(),
        "the RT runtime can be initialized only once"
    );
    let slice_ticks = B::ticks_per_ms().saturating_mul(slice_ms.max(1));
    let mut runtime = Box::new(Runtime::new(slice_ticks));
    runtime
        .add_task_for::<B>(main_priority, main)
        .expect("RT main task requires a free stack");
    RUNTIME.set(GlobalPtr::new(Box::into_raw(runtime)));
    PREEMPTIONS.store(0, Ordering::Relaxed);
    B::init_timer(tick::<B>);

    loop {
        switch_to_next::<B>(disable_interrupts::<B>());
        let (live, ready, deadline) = with_runtime::<B, _>(|runtime| {
            (
                runtime
                    .policy
                    .live_task_count(),
                runtime
                    .policy
                    .ready_len(),
                runtime
                    .policy
                    .next_deadline(),
            )
        });
        if live == 0 {
            break;
        }
        if ready == 0 {
            B::set_deadline(deadline.unwrap_or(u64::MAX));
            B::wait_for_interrupt();
        }
    }
    B::set_deadline(u64::MAX);
    B::restore_interrupts(interrupts_were_on);
}

/// Add a task without allocating after the runtime has started.
pub fn spawn<B: TimerInterrupt>(priority: u8, entry: TaskEntry) -> Result<TaskId, Error> {
    let task = with_runtime::<B, _>(|runtime| {
        if runtime
            .current
            .is_none()
        {
            return Err(Error::InvalidState);
        }
        runtime.add_task_for::<B>(priority, entry)
    })?;
    let should_preempt = with_runtime::<B, _>(|runtime| {
        let Some(current) = runtime.current else {
            return false;
        };
        runtime
            .policy
            .highest_ready_priority()
            .is_some_and(|priority| {
                priority
                    > runtime
                        .policy
                        .effective_priority(current)
                        .unwrap_or(0)
            })
    });
    if should_preempt {
        yield_now::<B>();
    }
    Ok(task)
}

impl Runtime {
    fn add_task_for<B: TimerInterrupt>(
        &mut self,
        priority: u8,
        entry: TaskEntry,
    ) -> Result<TaskId, Error> {
        let stack = Stack::try_take().ok_or(Error::Capacity)?;
        let id = self
            .policy
            .register(priority)?;
        let slot = &mut self.slots[id.index()];
        assert!(
            slot.stack
                .is_none()
                && !slot.pending_reap,
            "RT task slot reused before its stack was reaped"
        );
        let context = unsafe { Context::new(stack.top(), task_entry::<B>, id.index() as *mut ()) };
        slot.context = context;
        slot.stack = Some(stack);
        slot.entry = Some(entry);
        Ok(id)
    }
}

/// Voluntarily rotate to the next ready task while preserving the previous IF state.
pub fn yield_now<B: TimerInterrupt>() {
    let span = disable_interrupts::<B>();
    with_runtime::<B, _>(|runtime| {
        let current = runtime
            .current
            .expect("yield_now requires an RT task");
        runtime
            .policy
            .yield_task(current)
            .expect("yield_now requires a running task");
    });
    switch_to_next::<B>(span);
}

pub fn sleep_until<B: TimerInterrupt>(deadline: u64) -> Result<(), Error> {
    let span = disable_interrupts::<B>();
    let result = with_runtime::<B, _>(|runtime| {
        let current = runtime
            .current
            .ok_or(Error::InvalidState)?;
        runtime
            .policy
            .sleep_until(current, B::ticks(), deadline)
    });
    if let Err(error) = result {
        restore_interrupts::<B>(span);
        return Err(error);
    }
    switch_to_next::<B>(span);
    Ok(())
}

pub fn sleep_ms<B: TimerInterrupt>(milliseconds: u64) -> Result<(), Error> {
    let delay = B::ticks_per_ms().saturating_mul(milliseconds);
    sleep_until::<B>(B::ticks().saturating_add(delay))
}

pub fn wait_event<B: TimerInterrupt>(event: u8) -> Result<(), Error> {
    let span = disable_interrupts::<B>();
    let result = with_runtime::<B, _>(|runtime| {
        let current = runtime
            .current
            .ok_or(Error::InvalidState)?;
        runtime
            .policy
            .wait_event(current, event)
    });
    match result {
        Ok(true) => restore_interrupts::<B>(span),
        Ok(false) => {
            switch_to_next::<B>(span);
        }
        Err(error) => {
            restore_interrupts::<B>(span);
            return Err(error);
        }
    }
    Ok(())
}

/// Signal a bounded event. An interrupt handler must acknowledge its device before calling
/// this function, because a higher-priority wake may switch tasks before it returns.
pub fn signal_event<B: TimerInterrupt>(event: u8) -> Result<usize, Error> {
    let span = disable_interrupts::<B>();
    let result = with_runtime::<B, _>(|runtime| {
        runtime
            .policy
            .signal_event(event)
    });
    let woken = match result {
        Ok(woken) => woken,
        Err(error) => {
            restore_interrupts::<B>(span);
            return Err(error);
        }
    };
    let should_preempt = with_runtime::<B, _>(|runtime| {
        let Some(current) = runtime.current else {
            return woken != 0
                && runtime
                    .policy
                    .ready_len()
                    != 0;
        };
        runtime
            .policy
            .highest_ready_priority()
            .is_some_and(|priority| {
                priority
                    > runtime
                        .policy
                        .effective_priority(current)
                        .unwrap_or(0)
            })
    });
    if should_preempt {
        with_runtime::<B, _>(|runtime| {
            if let Some(current) = runtime.current {
                runtime
                    .policy
                    .yield_task(current)
                    .expect("preemption requires a running task");
            }
        });
        switch_to_next::<B>(span);
    } else {
        restore_interrupts::<B>(span);
    }
    Ok(woken)
}

pub fn create_mutex<B: TimerInterrupt>() -> Result<MutexId, Error> {
    with_runtime::<B, _>(|runtime| {
        runtime
            .policy
            .create_mutex()
    })
}

pub fn lock<B: TimerInterrupt>(mutex: MutexId) -> Result<(), Error> {
    let span = disable_interrupts::<B>();
    let result = with_runtime::<B, _>(|runtime| {
        let current = runtime
            .current
            .ok_or(Error::InvalidState)?;
        runtime
            .policy
            .lock(current, mutex)
    });
    match result {
        Ok(true) => restore_interrupts::<B>(span),
        Ok(false) => {
            switch_to_next::<B>(span);
        }
        Err(error) => {
            restore_interrupts::<B>(span);
            return Err(error);
        }
    }
    Ok(())
}

pub fn unlock<B: TimerInterrupt>(mutex: MutexId) -> Result<(), Error> {
    let span = disable_interrupts::<B>();
    let result = with_runtime::<B, _>(|runtime| {
        let current = runtime
            .current
            .ok_or(Error::InvalidState)?;
        runtime
            .policy
            .unlock(current, mutex)
    });
    if let Err(error) = result {
        restore_interrupts::<B>(span);
        return Err(error);
    }
    let should_preempt = with_runtime::<B, _>(|runtime| {
        let Some(current) = runtime.current else {
            return false;
        };
        runtime
            .policy
            .highest_ready_priority()
            .is_some_and(|priority| {
                priority
                    > runtime
                        .policy
                        .effective_priority(current)
                        .unwrap_or(0)
            })
    });
    if should_preempt {
        with_runtime::<B, _>(|runtime| {
            let current = runtime
                .current
                .expect("preemption requires a running task");
            runtime
                .policy
                .yield_task(current)
                .expect("preemption requires a running task");
        });
        switch_to_next::<B>(span);
    } else {
        restore_interrupts::<B>(span);
    }
    Ok(())
}

pub fn preemptions() -> usize {
    PREEMPTIONS.load(Ordering::Relaxed)
}

pub fn max_interrupts_off_ticks() -> u64 {
    MAX_INTERRUPTS_OFF_TICKS.load(Ordering::Relaxed)
}

pub fn current_effective_priority<B: TimerInterrupt>() -> Result<u8, Error> {
    with_runtime::<B, _>(|runtime| {
        let current = runtime
            .current
            .ok_or(Error::InvalidState)?;
        runtime
            .policy
            .effective_priority(current)
    })
}
