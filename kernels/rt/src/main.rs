#![no_std]
#![no_main]

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use green_tea_bare::{Board, Global, Platform, boot, println};
use green_tea_rt::{AdmissionError, DeadlineStats, MutexId, TaskSpec, admit_fixed_priority};
use green_tea_rt_bare as rt;

static LOCK: Global<Option<MutexId>> = Global::new(None);
static LOW_ACQUIRED: AtomicBool = AtomicBool::new(false);
static LOW_INHERITED: AtomicBool = AtomicBool::new(false);
static LOW_RELEASED: AtomicBool = AtomicBool::new(false);
static HIGH_COMPLETED: AtomicBool = AtomicBool::new(false);
static EARLY_EVENT_CONSUMED: AtomicBool = AtomicBool::new(false);
static BLOCKED_EVENT_WOKEN: AtomicBool = AtomicBool::new(false);
static MEDIUM_STEPS: AtomicUsize = AtomicUsize::new(0);

#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    let _framebuffer = boot("green-tea-rt");
    check_admission_and_deadline_reporting();
    rt::run::<Board>(main_task, 1, 1);
    assert!(
        LOW_ACQUIRED.load(Ordering::Acquire),
        "low-priority task never acquired its mutex"
    );
    assert!(
        LOW_INHERITED.load(Ordering::Acquire),
        "priority inheritance did not reach the lock owner"
    );
    assert!(
        LOW_RELEASED.load(Ordering::Acquire),
        "low-priority task did not release its mutex"
    );
    assert!(
        HIGH_COMPLETED.load(Ordering::Acquire),
        "high-priority waiter did not complete"
    );
    assert!(
        EARLY_EVENT_CONSUMED.load(Ordering::Acquire),
        "signal-before-wait was lost"
    );
    assert!(
        BLOCKED_EVENT_WOKEN.load(Ordering::Acquire),
        "blocked event waiter was not woken"
    );
    assert!(
        MEDIUM_STEPS.load(Ordering::Relaxed) != 0,
        "medium-priority interference was not exercised"
    );
    assert!(
        rt::preemptions() != 0,
        "timer did not preempt the busy task"
    );
    println!(
        "PASS: fixed priorities, admission, timer wakeup, transitive inheritance; max interrupts-off {} ticks",
        rt::max_interrupts_off_ticks(),
    );
    Board::halt()
}

fn check_admission_and_deadline_reporting() {
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

    let mut deadlines = DeadlineStats::default();
    assert_eq!(deadlines.report_completion(11, 10), Ok(true));
    assert_eq!(deadlines.misses, 1);
    println!("rt: synthetic admission accepted/rejected and deadline miss reported");
}

fn main_task() {
    rt::signal_event::<Board>(9).expect("latch event before waiter creation");
    rt::spawn::<Board>(2, early_event_waiter).expect("spawn early event waiter");
    rt::spawn::<Board>(3, blocked_event_waiter).expect("spawn blocked event waiter");
    rt::signal_event::<Board>(10).expect("wake blocked event waiter");

    let lock = rt::create_mutex::<Board>().expect("allocate bounded mutex");
    LOCK.set(Some(lock));
    rt::spawn::<Board>(2, low_task).expect("spawn low task");
    rt::spawn::<Board>(7, high_task).expect("spawn high waiter");
    rt::spawn::<Board>(4, medium_task).expect("spawn medium interferer");
}

fn early_event_waiter() {
    rt::wait_event::<Board>(9).expect("consume latched event");
    EARLY_EVENT_CONSUMED.store(true, Ordering::Release);
}

fn blocked_event_waiter() {
    rt::wait_event::<Board>(10).expect("wait for event");
    BLOCKED_EVENT_WOKEN.store(true, Ordering::Release);
}

fn lock_id() -> MutexId {
    LOCK.get()
        .expect("main task published mutex")
}

fn low_task() {
    rt::lock::<Board>(lock_id()).expect("low task locks mutex");
    LOW_ACQUIRED.store(true, Ordering::Release);
    println!("rt: low priority holds mutex and sleeps");
    rt::sleep_ms::<Board>(50).expect("low task sleeps until timer deadline");
    let inherited = rt::current_effective_priority::<Board>().expect("low task remains current");
    assert_eq!(
        inherited, 7,
        "high waiter priority must be inherited before unlock"
    );
    LOW_INHERITED.store(true, Ordering::Release);
    rt::unlock::<Board>(lock_id()).expect("low task releases mutex");
    LOW_RELEASED.store(true, Ordering::Release);
    println!("rt: inherited owner released mutex");
}

fn high_task() {
    rt::lock::<Board>(lock_id()).expect("high task waits for mutex");
    rt::unlock::<Board>(lock_id()).expect("high task releases mutex");
    HIGH_COMPLETED.store(true, Ordering::Release);
    println!("rt: high-priority waiter completed");
}

fn medium_task() {
    println!("rt: medium-priority interferer started");
    while !HIGH_COMPLETED.load(Ordering::Acquire) {
        MEDIUM_STEPS.fetch_add(1, Ordering::Relaxed);
    }
}
