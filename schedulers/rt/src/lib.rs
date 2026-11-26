#![no_std]

/// A task slot in a scheduler instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskId(u16);

impl TaskId {
    pub const fn from_index(index: usize) -> Option<Self> {
        if index <= u16::MAX as usize {
            Some(Self(index as u16))
        } else {
            None
        }
    }

    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// A mutex slot in a scheduler instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MutexId(u16);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskState {
    Free,
    Ready,
    Running,
    Sleeping,
    WaitingEvent,
    WaitingMutex,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidConfiguration,
    Capacity,
    NoReadyTask,
    InvalidTask,
    InvalidState,
    InvalidPriority,
    InvalidMutex,
    InvalidEvent,
    NotOwner,
    Deadlock,
    TaskHoldsMutex,
}

#[derive(Clone, Copy)]
struct Task {
    state: TaskState,
    base_priority: u8,
    effective_priority: u8,
    next_ready: Option<TaskId>,
    ready_sequence: u64,
    wait_sequence: u64,
    event: Option<u8>,
    waiting_mutex: Option<MutexId>,
}

impl Task {
    const EMPTY: Self = Self {
        state: TaskState::Free,
        base_priority: 0,
        effective_priority: 0,
        next_ready: None,
        ready_sequence: 0,
        wait_sequence: 0,
        event: None,
        waiting_mutex: None,
    };
}

#[derive(Clone, Copy)]
struct Mutex {
    owner: Option<TaskId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Timer {
    deadline: u64,
    task: TaskId,
}

impl Timer {
    const EMPTY: Self = Self {
        deadline: u64::MAX,
        task: TaskId(u16::MAX),
    };
}

/// Bounded scheduling state: `P` priorities, `N` task records, and at most one timer per
/// task. Higher numeric priorities run first; equal priorities are FIFO.
pub struct Scheduler<const N: usize, const P: usize = 64, const M: usize = N> {
    tasks: [Task; N],
    mutexes: [Option<Mutex>; M],
    mutex_count: usize,
    ready_head: [Option<TaskId>; P],
    ready_tail: [Option<TaskId>; P],
    ready_bitmap: u64,
    ready_sequence: u64,
    wait_sequence: u64,
    timers: [Timer; N],
    timer_count: usize,
    pending_events: u64,
}

impl<const N: usize, const P: usize, const M: usize> Scheduler<N, P, M> {
    pub fn new() -> Result<Self, Error> {
        if N == 0 || N > u16::MAX as usize || P == 0 || P > 64 || M > u16::MAX as usize {
            return Err(Error::InvalidConfiguration);
        }
        Ok(Self {
            tasks: [Task::EMPTY; N],
            mutexes: [None; M],
            mutex_count: 0,
            ready_head: [None; P],
            ready_tail: [None; P],
            ready_bitmap: 0,
            ready_sequence: 0,
            wait_sequence: 0,
            timers: [Timer::EMPTY; N],
            timer_count: 0,
            pending_events: 0,
        })
    }

    pub fn register(&mut self, priority: u8) -> Result<TaskId, Error> {
        if usize::from(priority) >= P {
            return Err(Error::InvalidPriority);
        }
        let Some(index) = self
            .tasks
            .iter()
            .position(|task| task.state == TaskState::Free)
        else {
            return Err(Error::Capacity);
        };
        let id = TaskId(index as u16);
        self.tasks[index] = Task {
            state: TaskState::Ready,
            base_priority: priority,
            effective_priority: priority,
            ..Task::EMPTY
        };
        self.enqueue(id);
        Ok(id)
    }

    /// Remove and return the highest-priority ready task, FIFO within that priority.
    pub fn next_ready(&mut self) -> Result<TaskId, Error> {
        if self.ready_bitmap == 0 {
            return Err(Error::NoReadyTask);
        }
        let priority = (63
            - self
                .ready_bitmap
                .leading_zeros()) as usize;
        let id = self.ready_head[priority].ok_or(Error::InvalidState)?;
        let task = &mut self.tasks[id.index()];
        self.ready_head[priority] = task.next_ready;
        task.next_ready = None;
        if self.ready_head[priority].is_none() {
            self.ready_tail[priority] = None;
            self.ready_bitmap &= !(1u64 << priority);
        }
        task.state = TaskState::Running;
        Ok(id)
    }

    pub fn yield_task(&mut self, id: TaskId) -> Result<(), Error> {
        self.require_state(id, TaskState::Running)?;
        self.enqueue(id);
        Ok(())
    }

    pub fn finish(&mut self, id: TaskId) -> Result<(), Error> {
        self.require_state(id, TaskState::Running)?;
        if self.mutexes[..self.mutex_count]
            .iter()
            .flatten()
            .any(|mutex| mutex.owner == Some(id))
        {
            return Err(Error::TaskHoldsMutex);
        }
        let task = &mut self.tasks[id.index()];
        *task = Task::EMPTY;
        self.recompute_priorities();
        self.rebuild_ready_queues();
        Ok(())
    }

    /// Block the running task until `deadline`, an absolute monotonic tick value.
    pub fn sleep_until(&mut self, id: TaskId, now: u64, deadline: u64) -> Result<(), Error> {
        self.require_state(id, TaskState::Running)?;
        if deadline <= now {
            self.enqueue(id);
            return Ok(());
        }
        if self.timer_count == N {
            return Err(Error::Capacity);
        }
        self.tasks[id.index()].state = TaskState::Sleeping;
        let index = self.timer_count;
        self.timer_count += 1;
        self.timers[index] = Timer { deadline, task: id };
        self.sift_up(index);
        Ok(())
    }

    /// Wake all sleepers whose deadlines are no later than `now`. Processing `k` timers
    /// costs `O(k log N)` and does no allocation.
    pub fn wake_expired(&mut self, now: u64) -> usize {
        let mut woken = 0;
        while self.timer_count != 0 && self.timers[0].deadline <= now {
            let timer = self.pop_timer();
            let task = &self.tasks[timer
                .task
                .index()];
            if task.state == TaskState::Sleeping {
                self.enqueue(timer.task);
                woken += 1;
            }
        }
        woken
    }

    pub fn next_deadline(&self) -> Option<u64> {
        (self.timer_count != 0).then_some(self.timers[0].deadline)
    }

    /// Return `true` if a signal was already pending; otherwise block the task.
    pub fn wait_event(&mut self, id: TaskId, event: u8) -> Result<bool, Error> {
        self.require_state(id, TaskState::Running)?;
        if event >= 64 {
            return Err(Error::InvalidEvent);
        }
        let bit = 1u64 << event;
        if self.pending_events & bit != 0 {
            self.pending_events &= !bit;
            return Ok(true);
        }
        let task = &mut self.tasks[id.index()];
        task.state = TaskState::WaitingEvent;
        task.event = Some(event);
        Ok(false)
    }

    /// Signal a bounded set of waiters. The state transition is done before returning, so
    /// a runtime can mask interrupts around its event check and idle handoff without a lost
    /// wakeup.
    pub fn signal_event(&mut self, event: u8) -> Result<usize, Error> {
        if event >= 64 {
            return Err(Error::InvalidEvent);
        }
        let mut woken = 0;
        for index in 0..N {
            if self.tasks[index].state == TaskState::WaitingEvent
                && self.tasks[index].event == Some(event)
            {
                self.tasks[index].event = None;
                self.enqueue(TaskId(index as u16));
                woken += 1;
            }
        }
        if woken == 0 {
            self.pending_events |= 1u64 << event;
        }
        Ok(woken)
    }

    pub fn create_mutex(&mut self) -> Result<MutexId, Error> {
        if self.mutex_count == M {
            return Err(Error::Capacity);
        }
        let id = MutexId(self.mutex_count as u16);
        self.mutexes[self.mutex_count] = Some(Mutex { owner: None });
        self.mutex_count += 1;
        Ok(id)
    }

    /// Acquire a free mutex, or block the running task and propagate priority inheritance.
    /// Returns `true` when acquired and `false` when the task was blocked.
    pub fn lock(&mut self, id: TaskId, mutex: MutexId) -> Result<bool, Error> {
        self.require_state(id, TaskState::Running)?;
        let index = self.mutex_index(mutex)?;
        let Some(owner) = self.mutexes[index]
            .expect("initialized mutex")
            .owner
        else {
            self.mutexes[index]
                .as_mut()
                .expect("initialized mutex")
                .owner = Some(id);
            return Ok(true);
        };
        if owner == id || self.would_deadlock(id, owner) {
            return Err(Error::Deadlock);
        }
        self.wait_sequence = self
            .wait_sequence
            .wrapping_add(1);
        let task = &mut self.tasks[id.index()];
        task.state = TaskState::WaitingMutex;
        task.waiting_mutex = Some(mutex);
        task.wait_sequence = self.wait_sequence;
        self.recompute_priorities();
        self.rebuild_ready_queues();
        Ok(false)
    }

    /// Release a mutex, transferring ownership to its highest-effective-priority waiter.
    pub fn unlock(&mut self, id: TaskId, mutex: MutexId) -> Result<Option<TaskId>, Error> {
        self.require_state(id, TaskState::Running)?;
        let index = self.mutex_index(mutex)?;
        if self.mutexes[index]
            .expect("initialized mutex")
            .owner
            != Some(id)
        {
            return Err(Error::NotOwner);
        }
        let next = self.best_mutex_waiter(mutex);
        self.mutexes[index]
            .as_mut()
            .expect("initialized mutex")
            .owner = next;
        if let Some(waiter) = next {
            self.tasks[waiter.index()].waiting_mutex = None;
            self.enqueue(waiter);
        }
        self.recompute_priorities();
        self.rebuild_ready_queues();
        Ok(next)
    }

    pub fn state(&self, id: TaskId) -> Result<TaskState, Error> {
        Ok(self
            .task(id)?
            .state)
    }

    pub fn base_priority(&self, id: TaskId) -> Result<u8, Error> {
        Ok(self
            .task(id)?
            .base_priority)
    }

    pub fn effective_priority(&self, id: TaskId) -> Result<u8, Error> {
        Ok(self
            .task(id)?
            .effective_priority)
    }

    pub fn ready_len(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| task.state == TaskState::Ready)
            .count()
    }

    pub fn timer_len(&self) -> usize {
        self.timer_count
    }

    pub fn live_task_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| task.state != TaskState::Free)
            .count()
    }

    pub fn highest_ready_priority(&self) -> Option<u8> {
        if self.ready_bitmap == 0 {
            None
        } else {
            Some(
                (63 - self
                    .ready_bitmap
                    .leading_zeros()) as u8,
            )
        }
    }

    fn task(&self, id: TaskId) -> Result<&Task, Error> {
        let task = self
            .tasks
            .get(id.index())
            .ok_or(Error::InvalidTask)?;
        if task.state == TaskState::Free {
            Err(Error::InvalidTask)
        } else {
            Ok(task)
        }
    }

    fn require_state(&self, id: TaskId, state: TaskState) -> Result<(), Error> {
        if self
            .task(id)?
            .state
            == state
        {
            Ok(())
        } else {
            Err(Error::InvalidState)
        }
    }

    fn mutex_index(&self, id: MutexId) -> Result<usize, Error> {
        let index = id.0 as usize;
        if index < self.mutex_count && self.mutexes[index].is_some() {
            Ok(index)
        } else {
            Err(Error::InvalidMutex)
        }
    }

    fn enqueue(&mut self, id: TaskId) {
        self.ready_sequence = self
            .ready_sequence
            .wrapping_add(1);
        self.tasks[id.index()].ready_sequence = self.ready_sequence;
        self.enqueue_preserving_order(id);
    }

    fn enqueue_preserving_order(&mut self, id: TaskId) {
        let index = id.index();
        let priority = self.tasks[index].effective_priority as usize;
        self.tasks[index].state = TaskState::Ready;
        self.tasks[index].next_ready = None;
        if let Some(tail) = self.ready_tail[priority] {
            self.tasks[tail.index()].next_ready = Some(id);
        } else {
            self.ready_head[priority] = Some(id);
        }
        self.ready_tail[priority] = Some(id);
        self.ready_bitmap |= 1u64 << priority;
    }

    fn rebuild_ready_queues(&mut self) {
        let mut ordered = [TaskId(u16::MAX); N];
        let mut count = 0;
        for index in 0..N {
            if self.tasks[index].state == TaskState::Ready {
                let id = TaskId(index as u16);
                let mut insert = count;
                while insert > 0
                    && self.tasks[ordered[insert - 1].index()].ready_sequence
                        > self.tasks[index].ready_sequence
                {
                    ordered[insert] = ordered[insert - 1];
                    insert -= 1;
                }
                ordered[insert] = id;
                count += 1;
            }
        }
        self.ready_head
            .fill(None);
        self.ready_tail
            .fill(None);
        self.ready_bitmap = 0;
        for id in ordered[..count]
            .iter()
            .copied()
        {
            self.enqueue_preserving_order(id);
        }
    }

    fn timer_less(left: Timer, right: Timer) -> bool {
        left.deadline < right.deadline
            || (left.deadline == right.deadline
                && left
                    .task
                    .0
                    < right
                        .task
                        .0)
    }

    fn swap_timers(&mut self, left: usize, right: usize) {
        self.timers
            .swap(left, right);
    }

    fn sift_up(&mut self, mut index: usize) {
        while index != 0 {
            let parent = (index - 1) / 2;
            if !Self::timer_less(self.timers[index], self.timers[parent]) {
                break;
            }
            self.swap_timers(index, parent);
            index = parent;
        }
    }

    fn sift_down(&mut self, mut index: usize) {
        loop {
            let left = index * 2 + 1;
            let right = left + 1;
            let mut smallest = index;
            if left < self.timer_count && Self::timer_less(self.timers[left], self.timers[smallest])
            {
                smallest = left;
            }
            if right < self.timer_count
                && Self::timer_less(self.timers[right], self.timers[smallest])
            {
                smallest = right;
            }
            if smallest == index {
                break;
            }
            self.swap_timers(index, smallest);
            index = smallest;
        }
    }

    fn pop_timer(&mut self) -> Timer {
        let first = self.timers[0];
        self.timer_count -= 1;
        if self.timer_count != 0 {
            self.timers[0] = self.timers[self.timer_count];
            self.timers[self.timer_count] = Timer::EMPTY;
            self.sift_down(0);
        } else {
            self.timers[0] = Timer::EMPTY;
        }
        first
    }

    fn best_mutex_waiter(&self, mutex: MutexId) -> Option<TaskId> {
        let mut best: Option<TaskId> = None;
        for index in 0..N {
            let task = &self.tasks[index];
            if task.state != TaskState::WaitingMutex || task.waiting_mutex != Some(mutex) {
                continue;
            }
            let candidate = TaskId(index as u16);
            if best.is_none_or(|current| {
                let old = &self.tasks[current.index()];
                task.effective_priority > old.effective_priority
                    || (task.effective_priority == old.effective_priority
                        && task.wait_sequence < old.wait_sequence)
            }) {
                best = Some(candidate);
            }
        }
        best
    }

    fn would_deadlock(&self, waiter: TaskId, mut owner: TaskId) -> bool {
        for _ in 0..N {
            if owner == waiter {
                return true;
            }
            let Some(waiting) = self.tasks[owner.index()].waiting_mutex else {
                return false;
            };
            let Some(next) = self.mutexes[waiting.0 as usize].and_then(|mutex| mutex.owner) else {
                return false;
            };
            owner = next;
        }
        true
    }

    fn recompute_priorities(&mut self) {
        for task in &mut self.tasks {
            if task.state != TaskState::Free {
                task.effective_priority = task.base_priority;
            }
        }
        for _ in 0..N {
            let mut changed = false;
            for mutex in self.mutexes[..self.mutex_count]
                .iter()
                .flatten()
            {
                let Some(owner) = mutex.owner else { continue };
                let inherited = self
                    .tasks
                    .iter()
                    .filter(|task| {
                        task.state == TaskState::WaitingMutex
                            && task
                                .waiting_mutex
                                .is_some_and(|id| {
                                    self.mutexes[id.0 as usize].and_then(|m| m.owner) == Some(owner)
                                })
                    })
                    .map(|task| task.effective_priority)
                    .max()
                    .unwrap_or(0);
                let task = &mut self.tasks[owner.index()];
                if inherited > task.effective_priority {
                    task.effective_priority = inherited;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }
}

/// A periodic, independent task in the single-CPU constrained-deadline model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskSpec {
    pub priority: u8,
    pub execution: u64,
    pub period: u64,
    pub deadline: u64,
    pub blocking: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionError {
    InvalidTaskSet,
    DuplicatePriority,
    ArithmeticOverflow,
    Unschedulable { priority: u8 },
    Inconclusive { priority: u8 },
}

/// Check fixed-priority response-time bounds for independent, single-CPU periodic tasks
/// with `0 < deadline <= period`. `interference` is a declared per-job bound for scheduler
/// and interrupt work. Measured execution times are not automatically safe bounds.
pub fn admit_fixed_priority(tasks: &[TaskSpec], interference: u64) -> Result<(), AdmissionError> {
    for (index, task) in tasks
        .iter()
        .enumerate()
    {
        if task.execution == 0
            || task.period == 0
            || task.deadline == 0
            || task.deadline > task.period
        {
            return Err(AdmissionError::InvalidTaskSet);
        }
        if tasks[..index]
            .iter()
            .any(|other| other.priority == task.priority)
        {
            return Err(AdmissionError::DuplicatePriority);
        }
    }

    for task in tasks {
        let base = task
            .execution
            .checked_add(task.blocking)
            .and_then(|value| value.checked_add(interference))
            .ok_or(AdmissionError::ArithmeticOverflow)?;
        if base > task.deadline {
            return Err(AdmissionError::Unschedulable {
                priority: task.priority,
            });
        }
        let mut response = base;
        let mut converged = false;
        for _ in 0..64 {
            let mut next = base;
            for higher in tasks
                .iter()
                .filter(|higher| higher.priority > task.priority)
            {
                let jobs = response / higher.period + u64::from(response % higher.period != 0);
                let interference = jobs
                    .checked_mul(higher.execution)
                    .ok_or(AdmissionError::ArithmeticOverflow)?;
                next = next
                    .checked_add(interference)
                    .ok_or(AdmissionError::ArithmeticOverflow)?;
            }
            if next > task.deadline {
                return Err(AdmissionError::Unschedulable {
                    priority: task.priority,
                });
            }
            if next == response {
                converged = true;
                break;
            }
            response = next;
        }
        if !converged {
            return Err(AdmissionError::Inconclusive {
                priority: task.priority,
            });
        }
    }
    Ok(())
}

/// Bounded accounting for completion times against caller-supplied absolute deadlines.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeadlineStats {
    pub jobs: u64,
    pub misses: u64,
}

impl DeadlineStats {
    /// Record one completion; equality with the absolute deadline is on time.
    pub fn report_completion(
        &mut self,
        completed_at: u64,
        deadline: u64,
    ) -> Result<bool, AdmissionError> {
        let missed = completed_at > deadline;
        let jobs = self
            .jobs
            .checked_add(1)
            .ok_or(AdmissionError::ArithmeticOverflow)?;
        let misses = if missed {
            self.misses
                .checked_add(1)
                .ok_or(AdmissionError::ArithmeticOverflow)?
        } else {
            self.misses
        };
        self.jobs = jobs;
        self.misses = misses;
        Ok(missed)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AdmissionError, DeadlineStats, Error, Scheduler, TaskSpec, TaskState, admit_fixed_priority,
    };

    #[test]
    fn priority_bitmap_and_fifo_order() {
        let mut scheduler = Scheduler::<8>::new().unwrap();
        let low1 = scheduler
            .register(1)
            .unwrap();
        let high1 = scheduler
            .register(7)
            .unwrap();
        let high2 = scheduler
            .register(7)
            .unwrap();
        let low2 = scheduler
            .register(1)
            .unwrap();
        assert_eq!(scheduler.next_ready(), Ok(high1));
        assert_eq!(scheduler.next_ready(), Ok(high2));
        assert_eq!(scheduler.next_ready(), Ok(low1));
        assert_eq!(scheduler.next_ready(), Ok(low2));
        assert_eq!(scheduler.next_ready(), Err(Error::NoReadyTask));
    }

    #[test]
    fn task_and_priority_capacity_are_checked() {
        let mut scheduler = Scheduler::<1, 4>::new().unwrap();
        assert_eq!(scheduler.register(4), Err(Error::InvalidPriority));
        let task = scheduler
            .register(3)
            .unwrap();
        assert_eq!(scheduler.register(0), Err(Error::Capacity));
        assert_eq!(scheduler.state(task), Ok(TaskState::Ready));
    }

    #[test]
    fn timers_wake_in_deadline_order_and_handle_equal_deadlines() {
        let mut scheduler = Scheduler::<4>::new().unwrap();
        let first = scheduler
            .register(1)
            .unwrap();
        let second = scheduler
            .register(2)
            .unwrap();
        assert_eq!(scheduler.next_ready(), Ok(second));
        scheduler
            .sleep_until(second, 10, 20)
            .unwrap();
        assert_eq!(scheduler.next_ready(), Ok(first));
        scheduler
            .sleep_until(first, 10, 20)
            .unwrap();
        assert_eq!(scheduler.next_deadline(), Some(20));
        assert_eq!(scheduler.wake_expired(19), 0);
        assert_eq!(scheduler.wake_expired(20), 2);
        assert_eq!(scheduler.next_ready(), Ok(second));
        assert_eq!(scheduler.next_ready(), Ok(first));
    }

    #[test]
    fn event_signal_wakes_all_waiters_without_allocating() {
        let mut scheduler = Scheduler::<4>::new().unwrap();
        let a = scheduler
            .register(1)
            .unwrap();
        let b = scheduler
            .register(2)
            .unwrap();
        assert_eq!(scheduler.next_ready(), Ok(b));
        assert_eq!(scheduler.wait_event(b, 7), Ok(false));
        assert_eq!(scheduler.next_ready(), Ok(a));
        assert_eq!(scheduler.wait_event(a, 7), Ok(false));
        assert_eq!(scheduler.signal_event(7), Ok(2));
        assert_eq!(scheduler.signal_event(7), Ok(0));
        assert_eq!(scheduler.next_ready(), Ok(b));
        assert_eq!(scheduler.next_ready(), Ok(a));
    }

    #[test]
    fn signal_before_wait_is_latched_once() {
        let mut scheduler = Scheduler::<2>::new().unwrap();
        let task = scheduler
            .register(1)
            .unwrap();
        assert_eq!(scheduler.next_ready(), Ok(task));
        assert_eq!(scheduler.signal_event(3), Ok(0));
        assert_eq!(scheduler.wait_event(task, 3), Ok(true));
        assert_eq!(scheduler.state(task), Ok(TaskState::Running));
        assert_eq!(scheduler.signal_event(64), Err(Error::InvalidEvent));
    }

    #[test]
    fn mutex_inheritance_propagates_transitively_and_restores() {
        let mut scheduler = Scheduler::<4, 8, 4>::new().unwrap();
        let low = scheduler
            .register(1)
            .unwrap();
        assert_eq!(scheduler.next_ready(), Ok(low));
        let a = scheduler
            .create_mutex()
            .unwrap();
        let b = scheduler
            .create_mutex()
            .unwrap();
        assert_eq!(scheduler.lock(low, a), Ok(true));
        scheduler
            .yield_task(low)
            .unwrap();

        let middle = scheduler
            .register(2)
            .unwrap();
        assert_eq!(scheduler.next_ready(), Ok(middle));
        assert_eq!(scheduler.lock(middle, b), Ok(true));
        assert_eq!(scheduler.lock(middle, a), Ok(false));
        assert_eq!(scheduler.effective_priority(low), Ok(2));
        let high = scheduler
            .register(7)
            .unwrap();
        assert_eq!(scheduler.next_ready(), Ok(high));
        assert_eq!(scheduler.lock(high, b), Ok(false));
        assert_eq!(scheduler.effective_priority(middle), Ok(7));
        assert_eq!(scheduler.effective_priority(low), Ok(7));
        assert_eq!(scheduler.next_ready(), Ok(low));
        assert_eq!(scheduler.unlock(low, a), Ok(Some(middle)));
        assert_eq!(scheduler.effective_priority(low), Ok(1));
        assert_eq!(scheduler.next_ready(), Ok(middle));
        assert_eq!(scheduler.unlock(middle, b), Ok(Some(high)));
        assert_eq!(scheduler.effective_priority(middle), Ok(2));
        assert_eq!(scheduler.next_ready(), Ok(high));
        assert_eq!(scheduler.effective_priority(high), Ok(7));
    }

    #[test]
    fn lock_cycles_and_wrong_owners_are_rejected() {
        let mut scheduler = Scheduler::<2, 8, 2>::new().unwrap();
        let a = scheduler
            .register(1)
            .unwrap();
        let first = scheduler
            .create_mutex()
            .unwrap();
        let second = scheduler
            .create_mutex()
            .unwrap();
        assert_eq!(scheduler.next_ready(), Ok(a));
        scheduler
            .lock(a, first)
            .unwrap();
        scheduler
            .yield_task(a)
            .unwrap();
        let b = scheduler
            .register(2)
            .unwrap();
        assert_eq!(scheduler.next_ready(), Ok(b));
        scheduler
            .lock(b, second)
            .unwrap();
        assert_eq!(scheduler.lock(b, first), Ok(false));
        assert_eq!(scheduler.next_ready(), Ok(a));
        assert_eq!(scheduler.lock(a, second), Err(Error::Deadlock));
        assert_eq!(scheduler.unlock(a, second), Err(Error::NotOwner));
    }

    #[test]
    fn response_time_admission_accepts_and_rejects_checked_task_sets() {
        let feasible = [
            TaskSpec {
                priority: 7,
                execution: 2,
                period: 10,
                deadline: 10,
                blocking: 0,
            },
            TaskSpec {
                priority: 3,
                execution: 4,
                period: 20,
                deadline: 20,
                blocking: 1,
            },
        ];
        assert_eq!(admit_fixed_priority(&feasible, 1), Ok(()));

        let infeasible = [
            TaskSpec {
                priority: 7,
                execution: 4,
                period: 5,
                deadline: 5,
                blocking: 0,
            },
            TaskSpec {
                priority: 3,
                execution: 4,
                period: 8,
                deadline: 8,
                blocking: 0,
            },
        ];
        assert_eq!(
            admit_fixed_priority(&infeasible, 0),
            Err(AdmissionError::Unschedulable { priority: 3 }),
        );
    }

    #[test]
    fn admission_rejects_unsupported_geometry_duplicates_and_overflow() {
        let invalid = [TaskSpec {
            priority: 1,
            execution: 1,
            period: 5,
            deadline: 6,
            blocking: 0,
        }];
        assert_eq!(
            admit_fixed_priority(&invalid, 0),
            Err(AdmissionError::InvalidTaskSet)
        );

        let duplicate = [
            TaskSpec {
                priority: 2,
                execution: 1,
                period: 5,
                deadline: 5,
                blocking: 0,
            },
            TaskSpec {
                priority: 2,
                execution: 1,
                period: 6,
                deadline: 6,
                blocking: 0,
            },
        ];
        assert_eq!(
            admit_fixed_priority(&duplicate, 0),
            Err(AdmissionError::DuplicatePriority)
        );

        let overflow = [TaskSpec {
            priority: 1,
            execution: u64::MAX,
            period: u64::MAX,
            deadline: u64::MAX,
            blocking: 1,
        }];
        assert_eq!(
            admit_fixed_priority(&overflow, 0),
            Err(AdmissionError::ArithmeticOverflow)
        );
    }

    #[test]
    fn deadline_reporting_counts_late_jobs_without_overflow() {
        let mut stats = DeadlineStats::default();
        assert_eq!(stats.report_completion(10, 10), Ok(false));
        assert_eq!(stats.report_completion(12, 11), Ok(true));
        assert_eq!(stats, DeadlineStats { jobs: 2, misses: 1 });
        stats.jobs = u64::MAX;
        assert_eq!(
            stats.report_completion(1, 2),
            Err(AdmissionError::ArithmeticOverflow)
        );
    }
}
