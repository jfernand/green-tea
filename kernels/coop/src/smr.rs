//! The test of safe memory reclamation, `cargo xtask kernel --kernel smr`.
//!
//! A lock-free stack and queue (green-tea-smr), with their nodes on the kernel's heap.
//! First on one CPU, a pop is interrupted at its worst moment by what another thread
//! could do there, under each way of freeing nodes. Then the kernel starts the board's
//! other CPUs and all four use one stack, and one queue, at once; one of them stops in
//! the middle of an operation while the others work; one replaces a value that the others
//! read; and a queue of jobs is run by all.
//!
//! Only the boot CPU prints. The others leave what they counted in atomics.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicUsize, Ordering};
use green_tea_bare::Framebuffer;
use green_tea_bare::screen::Screen;
use green_tea_bare::{Board, Platform, cpus, heap, println};
use green_tea_smr::epoch::{self, Epochs};
use green_tea_smr::hazard::{self, Hazards};
use green_tea_smr::queue::Queue;
use green_tea_smr::rcu::Rcu;
use green_tea_smr::stack::Stack;
use green_tea_smr::{AtOnce, Counts, Reclaim};

/// The CPUs `cargo xtask kernel` gives the machine for this test (`-smp 4`).
const CPUS: usize = 4;

// The counts below are small on purpose. All four CPUs are busy for as long as this test
// runs, `cargo xtask kernel` runs every kernel at once, and one of the others measures time.

/// How many values each CPU pushes and pops, or enqueues and dequeues.
const EACH: usize = 5_000;

/// How many pops and pushes each working CPU makes while another is stopped.
const WHILE_STOPPED: usize = 2_000;

/// The job queue counts the primes below this.
const LIMIT: u32 = 1 << 15;
/// A job for more numbers than this splits in two.
const SMALL: u32 = 1 << 8;

pub fn test(fb: Framebuffer) {
    match run() {
        Err(e) => println!("FAIL: {e}"),
        Ok(primes) => {
            Screen::new(fb).finish(0);
            println!(
                "PASS: an interrupted pop corrupted its stack when nodes were freed at once, and not with epochs or \
                 hazard pointers; {CPUS} CPUs lost no value on a stack or a queue, and found {primes} primes"
            );
        }
    }
    cpus::park();
}

fn run() -> Result<u32, &'static str> {
    let used = heap::used();
    interrupted()?;
    if heap::used() != used {
        return Err("the interrupted pops didn't leave the heap as they found it");
    }
    let running = cpus::start_all();
    println!("{running} CPUs are running: the one that booted, and {} it started", running - 1);
    if running != CPUS {
        return Err("the machine should have 4 CPUs: run it with -smp 4");
    }
    let used = heap::used();
    println!("each CPU pushes {EACH} values of its own on one stack, and pops one after each push:");
    together::<ByEpochs>("stack", stack_work::<ByEpochs>)?;
    together::<ByHazards>("stack", stack_work::<ByHazards>)?;
    println!("each CPU puts {EACH} values of its own in one queue, and takes one out after each:");
    together::<ByEpochs>("queue", queue_work::<ByEpochs>)?;
    together::<ByHazards>("queue", queue_work::<ByHazards>)?;
    println!(
        "CPU 3 stops in the middle of a pop, and the other three then make {WHILE_STOPPED} pops and pushes each. \
         when they are done:"
    );
    stopped::<ByEpochs>()?;
    stopped::<ByHazards>()?;
    replaced()?;
    let primes = jobs()?;
    if heap::used() != used {
        return Err("the heap didn't end as it began: a node was lost, or freed twice");
    }
    heap::with(|heap| heap.check())?;
    Ok(primes)
}

fn ms(ticks: u64) -> u64 {
    ticks / Board::ticks_per_ms()
}

// ---- A pop, interrupted ---------------------------------------------------------------

/// What happened when a pop was interrupted.
struct Interrupted {
    /// The stack before: three nodes' addresses and values, from the top.
    before: [(usize, usize); 3],
    /// Where the heap put the node pushed on the other stack, and the one pushed on this.
    other: usize,
    new: usize,
    /// How many times the pop read the head before its compare-and-swap succeeded.
    rounds: usize,
    popped: Option<usize>,
    /// The stack afterwards, and how many nodes it has.
    after: [(usize, usize); 4],
    left: usize,
    /// Whether the stack's first node is now the other stack's first node.
    shared: bool,
    /// What the interrupting thread's reclamation had done by the end.
    theirs: Counts,
}

/// Push 1, 2 and 3. Begin a pop, and between its read of the first node's `next` and its
/// compare-and-swap do what another thread could: pop twice, `look` for nodes to free,
/// push 9 on another stack, and push 4 on this one.
fn interrupt<R: Reclaim>(mine: &mut R, theirs: &mut R, look: fn(&mut R)) -> Interrupted {
    let (mut stack, mut other) = (Stack::new(), Stack::new());
    let lowest = stack.push(1usize);
    stack.push(2);
    stack.push(3);
    let mut before = [(0, 0); 3];
    stack.look(&mut before);
    let (mut rounds, mut other_node, mut new_node) = (0, 0, 0);
    let popped = stack.pop_paused(mine, || {
        rounds += 1;
        if rounds == 1 {
            stack.pop(theirs);
            stack.pop(theirs);
            look(theirs);
            other_node = other.push(9usize) as usize;
            new_node = stack.push(4) as usize;
        }
    });
    let mut after = [(0, 0); 4];
    let left = stack.look(&mut after);
    let mut theirs_first = [(0, 0); 1];
    other.look(&mut theirs_first);
    let shared = left > 0 && after[0].0 == theirs_first[0].0;
    if shared {
        // The node is the other stack's to free. The node that held 1 is linked from
        // nowhere, and nobody else will free it.
        stack.abandon();
        // SAFETY: as the comment says.
        unsafe { Stack::discard(lowest) };
    }
    Interrupted {
        before,
        other: other_node,
        new: new_node,
        rounds,
        popped,
        after,
        left,
        shared,
        theirs: theirs.counts(),
    }
}

/// The same interruption under each way of freeing nodes, printed. Addresses are printed
/// as offsets from the lowest of the three nodes.
fn interrupted() -> Result<(), &'static str> {
    println!("a stack holds 3, 2 and 1. a pop reads the first node's address and the second's, and before its");
    println!("compare-and-swap another thread pops twice, pushes 9 on another stack, and pushes 4 on this one.");
    let report = |name: &str, run: &Interrupted, between: &str| {
        let base = run.before.iter().map(|&(node, _)| node).min().unwrap_or(0);
        let at = |node: usize| node as isize - base as isize;
        let [first, second, third] = run.before;
        println!("  nodes freed {name}:");
        println!(
            "    the stack is {:+} ({}) -> {:+} ({}) -> {:+} ({}); the pop has read head = {:+} and next = {:+}",
            at(first.0),
            first.1,
            at(second.0),
            second.1,
            at(third.0),
            third.1,
            at(first.0),
            at(second.0)
        );
        println!(
            "    the other thread pops 3 and 2{between}; its 9 goes in a node at {:+}, and its 4 in one at {:+}",
            at(run.other),
            at(run.new)
        );
        println!(
            "    the compare-and-swap of head from {:+} to {:+} {}; the pop returns {}",
            at(first.0),
            at(second.0),
            if run.rounds == 1 { "succeeds" } else { "fails, and the pop starts again" },
            run.popped.unwrap_or(0)
        );
        match (run.left, run.shared) {
            (1, true) => println!(
                "    the stack is {:+} ({}), which is the other stack's node; 1 is on no stack",
                at(run.after[0].0),
                run.after[0].1
            ),
            (1, false) => println!("    the stack is {:+} ({})", at(run.after[0].0), run.after[0].1),
            _ => println!("    the stack has {} nodes", run.left),
        }
    };

    let at_once = interrupt(&mut AtOnce::default(), &mut AtOnce::default(), |_| ());
    report("at once", &at_once, ", and both nodes are freed");
    if !(at_once.rounds == 1 && at_once.shared && at_once.new == at_once.before[0].0) {
        return Err("freeing nodes at once didn't corrupt the stack: the heap gave out other addresses");
    }

    let epochs = Epochs::new();
    let by_epochs = interrupt(&mut epochs.local(0), &mut epochs.local(1), |_| ());
    report("by epochs", &by_epochs, ", and both nodes wait in a limbo bag");
    let sound = |run: &Interrupted| run.rounds == 2 && run.popped == Some(4) && run.left == 1 && run.after[0].1 == 1;
    if !sound(&by_epochs) || by_epochs.theirs.freed != 0 {
        return Err("an interrupted pop went wrong with epochs");
    }
    drop(epochs);

    let hazards = Hazards::new();
    let by_hazards = interrupt(&mut hazards.local(0), &mut hazards.local(1), |theirs| theirs.scan());
    report("by hazard pointers", &by_hazards, "; a hazard pointer names the first, and the second is freed");
    if !sound(&by_hazards) || by_hazards.theirs.freed != 1 {
        return Err("an interrupted pop went wrong with hazard pointers");
    }
    Ok(())
}

// ---- Four CPUs ------------------------------------------------------------------------

/// What the CPUs share in one experiment.
struct Shared {
    stack: Stack<usize>,
    queue: Queue<usize>,
    epochs: Epochs,
    hazards: Hazards,
    /// How many times each value has been popped or dequeued.
    seen: Vec<AtomicU8>,
    jobs: Queue<Job>,
    results: Stack<usize>,
    /// Two numbers, the second always twice the first.
    pair: Rcu<(usize, usize)>,
}

static SHARED: AtomicPtr<Shared> = AtomicPtr::new(core::ptr::null_mut());

/// What the CPUs counted, added up.
static RETIRED: AtomicUsize = AtomicUsize::new(0);
static FREED: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static LOOKED: AtomicUsize = AtomicUsize::new(0);
static OPERATIONS: AtomicUsize = AtomicUsize::new(0);
static WRONG: AtomicUsize = AtomicUsize::new(0);

fn shared() -> &'static Shared {
    // SAFETY: `with_shared` made it before the CPUs were given their work, and frees it
    // after they are done.
    unsafe { &*SHARED.load(Ordering::Acquire) }
}

fn tally(counts: Counts) {
    RETIRED.fetch_add(counts.retired, Ordering::Relaxed);
    FREED.fetch_add(counts.freed, Ordering::Relaxed);
    PEAK.fetch_add(counts.peak, Ordering::Relaxed);
    LOOKED.fetch_add(counts.looked, Ordering::Relaxed);
    OPERATIONS.fetch_add(counts.operations, Ordering::Relaxed);
}

fn tallied() -> Counts {
    Counts {
        retired: RETIRED.load(Ordering::Relaxed),
        freed: FREED.load(Ordering::Relaxed),
        peak: PEAK.load(Ordering::Relaxed),
        looked: LOOKED.load(Ordering::Relaxed),
        operations: OPERATIONS.load(Ordering::Relaxed),
        ..Counts::default()
    }
}

/// Make what the CPUs share, run `work` on every CPU, and hand what they shared to
/// `after`, with how long the work took. The CPUs' counts are in the statics.
fn with_shared<T>(
    values: usize,
    before: impl FnOnce(&Shared),
    work: fn(usize),
    after: impl FnOnce(&mut Shared, u64) -> T,
) -> T {
    for count in [&RETIRED, &FREED, &PEAK, &LOOKED, &OPERATIONS, &WRONG] {
        count.store(0, Ordering::Relaxed);
    }
    let shared = Box::into_raw(Box::new(Shared {
        stack: Stack::new(),
        queue: Queue::new(),
        epochs: Epochs::new(),
        hazards: Hazards::new(),
        seen: (0..values).map(|_| AtomicU8::new(0)).collect(),
        jobs: Queue::new(),
        results: Stack::new(),
        pair: Rcu::new((0, 0)),
    }));
    // SAFETY: ours alone until the next line.
    before(unsafe { &*shared });
    SHARED.store(shared, Ordering::Release);
    let start = Board::ticks();
    cpus::run(work);
    let took = ms(Board::ticks() - start);
    SHARED.store(core::ptr::null_mut(), Ordering::Release);
    // SAFETY: made by `Box::into_raw` above, and no CPU is using it any more.
    let mut shared = unsafe { Box::from_raw(shared) };
    after(&mut shared, took)
}

/// A way of freeing nodes, as the test's CPUs use it.
trait Scheme {
    const NAME: &'static str;
    type Local<'a>: Reclaim;
    fn local(shared: &Shared, cpu: usize) -> Self::Local<'_>;
    /// Nodes that the CPUs' `Local`s left behind, freed now that the CPUs are done.
    fn orphans(shared: &mut Shared) -> usize;
    /// Begin a pop on `stack`, and go no further than its first read.
    fn begin_a_pop(local: &mut Self::Local<'_>, stack: &Stack<usize>);
}

struct ByEpochs;
struct ByHazards;

impl Scheme for ByEpochs {
    const NAME: &'static str = "by epochs";
    type Local<'a> = epoch::Local<'a>;

    fn local(shared: &Shared, cpu: usize) -> epoch::Local<'_> {
        shared.epochs.local(cpu)
    }

    fn orphans(shared: &mut Shared) -> usize {
        shared.epochs.free_orphans()
    }

    fn begin_a_pop(local: &mut epoch::Local<'_>, _stack: &Stack<usize>) {
        local.enter();
    }
}

impl Scheme for ByHazards {
    const NAME: &'static str = "by hazard pointers";
    type Local<'a> = hazard::Local<'a>;

    fn local(shared: &Shared, cpu: usize) -> hazard::Local<'_> {
        shared.hazards.local(cpu)
    }

    fn orphans(shared: &mut Shared) -> usize {
        shared.hazards.free_orphans()
    }

    fn begin_a_pop(local: &mut hazard::Local<'_>, stack: &Stack<usize>) {
        local.enter();
        local.protect(0, stack.head());
    }
}

fn saw(value: usize) {
    match shared().seen.get(value) {
        Some(times) => drop(times.fetch_add(1, Ordering::Relaxed)),
        None => drop(WRONG.fetch_add(1, Ordering::Relaxed)),
    }
}

/// One CPU's work on the stack: push a value of its own and pop one, `EACH` times.
fn stack_work<S: Scheme>(cpu: usize) {
    let shared = shared();
    let mut smr = S::local(shared, cpu);
    for i in 0..EACH {
        shared.stack.push(cpu * EACH + i);
        match pop::<S>(&shared.stack, &mut smr) {
            Some(value) => saw(value),
            None => drop(WRONG.fetch_add(1, Ordering::Relaxed)),
        }
    }
    tally(smr.counts());
}

/// A pop that isn't inlined, one for each scheme, so that the kernel has a function to
/// disassemble.
#[inline(never)]
fn pop<S: Scheme>(stack: &Stack<usize>, smr: &mut S::Local<'_>) -> Option<usize> {
    stack.pop(smr)
}

/// The same with nodes freed at once, which is safe on a stack that one CPU has to itself.
#[inline(never)]
fn pop_at_once(stack: &Stack<usize>, smr: &mut AtOnce) -> Option<usize> {
    stack.pop(smr)
}

/// One CPU's work on the queue: enqueue a value of its own and dequeue one, `EACH`
/// times. Each CPU's values must come out in the order it put them in, whoever takes them.
fn queue_work<S: Scheme>(cpu: usize) {
    let shared = shared();
    let mut smr = S::local(shared, cpu);
    let mut last = [None; CPUS];
    for i in 0..EACH {
        shared.queue.enqueue(&mut smr, cpu * EACH + i);
        let Some(value) = shared.queue.dequeue(&mut smr) else {
            WRONG.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        saw(value);
        if let Some(last) = last.get_mut(value / EACH) {
            if last.is_some_and(|last| last >= value) {
                WRONG.fetch_add(1, Ordering::Relaxed);
            }
            *last = Some(value);
        }
    }
    tally(smr.counts());
}

/// All four CPUs on one structure, and what their reclamation did.
fn together<S: Scheme>(structure: &str, work: fn(usize)) -> Result<(), &'static str> {
    let (once, orphans, took) = with_shared(
        CPUS * EACH,
        |_| (),
        work,
        |shared, took| {
            let once = shared.seen.iter().all(|times| times.load(Ordering::Relaxed) == 1);
            (once, S::orphans(shared), took)
        },
    );
    let counts = tallied();
    println!(
        "  freed {:18} {} values came back, each once; {} nodes were retired, and {} freed",
        S::NAME,
        CPUS * EACH,
        counts.retired,
        counts.freed + orphans
    );
    println!(
        "  this run: {} of them were freed while the CPUs worked; at most {} waited; {took} ms",
        counts.freed, counts.peak
    );
    if WRONG.load(Ordering::Relaxed) != 0 || !once {
        return Err(match structure {
            "stack" => "a value was popped twice, or never, or wasn't one that was pushed",
            _ => "a value was dequeued twice, or never, or out of order",
        });
    }
    if counts.retired != CPUS * EACH || counts.freed + orphans != counts.retired {
        return Err("not every node that was unlinked was retired and freed, once");
    }
    Ok(())
}

// ---- One CPU stops --------------------------------------------------------------------

static STOPPED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicUsize = AtomicUsize::new(0);
static RESUME: AtomicBool = AtomicBool::new(false);
static HELD: AtomicUsize = AtomicUsize::new(0);

/// CPU 3 begins a pop and stops. The others pop a value and push it back,
/// `WHILE_STOPPED` times each, and report. When CPU 0 has seen what the heap holds,
/// CPU 3 goes on.
fn stopped_work<S: Scheme>(cpu: usize) {
    let shared = shared();
    let mut smr = S::local(shared, cpu);
    if cpu == CPUS - 1 {
        S::begin_a_pop(&mut smr, &shared.stack);
        STOPPED.store(true, Ordering::Release);
        while !RESUME.load(Ordering::Acquire) {
            spin_loop();
        }
        smr.exit();
        return;
    }
    while !STOPPED.load(Ordering::Acquire) {
        spin_loop();
    }
    for _ in 0..WHILE_STOPPED {
        match shared.stack.pop(&mut smr) {
            Some(value) => drop(shared.stack.push(value)),
            None => drop(WRONG.fetch_add(1, Ordering::Relaxed)),
        }
    }
    tally(smr.counts());
    DONE.fetch_add(1, Ordering::Release);
    if cpu == 0 {
        while DONE.load(Ordering::Acquire) < CPUS - 1 {
            spin_loop();
        }
        // What the retired nodes hold, and the lists of them.
        HELD.store(heap::used() - HELD.load(Ordering::Relaxed), Ordering::Relaxed);
        RESUME.store(true, Ordering::Release);
    }
    while !RESUME.load(Ordering::Acquire) {
        spin_loop();
    }
}

fn stopped<S: Scheme>() -> Result<(), &'static str> {
    for flag in [&STOPPED, &RESUME] {
        flag.store(false, Ordering::Release);
    }
    DONE.store(0, Ordering::Release);
    let orphans = with_shared(
        0,
        |shared| {
            for value in 0..CPUS - 1 {
                shared.stack.push(value);
            }
            HELD.store(heap::used(), Ordering::Relaxed);
        },
        stopped_work::<S>,
        |shared, _| S::orphans(shared),
    );
    let counts = tallied();
    let waiting = counts.retired - counts.freed;
    if S::NAME == ByEpochs::NAME {
        println!(
            "  freed {:18} {} nodes were retired, {} freed, and {waiting} waited: {} bytes of nodes",
            S::NAME,
            counts.retired,
            counts.freed,
            waiting * NODE
        );
    } else {
        println!(
            "  freed {:18} {} nodes were retired, and at most {} waited: {} bytes of nodes",
            S::NAME,
            counts.retired,
            counts.peak,
            counts.peak * NODE
        );
        println!("  this run: {} were freed, and {waiting} waited at the end", counts.freed);
    }
    println!("  this run: the heap had {} bytes more in use, with the lists of them", HELD.load(Ordering::Relaxed));
    if WRONG.load(Ordering::Relaxed) != 0 {
        return Err("a pop found the stack empty while a CPU was stopped");
    }
    let working = (CPUS - 1) * WHILE_STOPPED;
    if counts.retired != working || counts.freed + orphans != counts.retired {
        return Err("not every node that was unlinked while a CPU was stopped was retired and freed, once");
    }
    if S::NAME == ByEpochs::NAME && counts.freed != 0 {
        return Err("a node was freed while a CPU was in an operation that began before it was retired");
    }
    if S::NAME == ByHazards::NAME && !(1..=(CPUS - 1) * hazard::BATCH).contains(&waiting) {
        return Err("hazard pointers kept back no node, or more than three batches");
    }
    if S::NAME == ByHazards::NAME && counts.peak != (CPUS - 1) * hazard::BATCH {
        return Err("a CPU's list of retired nodes didn't stop at a batch");
    }
    Ok(())
}

/// What a node of the stack takes from the heap: 16 bytes, and the block's two tags.
const NODE: usize = 32;

// ---- A value that is replaced ----------------------------------------------------------

/// How many times CPU 0 replaces the pair.
const UPDATES: usize = 5_000;
static READS: AtomicUsize = AtomicUsize::new(0);

/// CPU 0 replaces the pair `UPDATES` times. The other CPUs read it until it holds the
/// last value, and check each time that they were given one pair and not halves of two.
fn replaced_work(cpu: usize) {
    let shared = shared();
    let mut smr = shared.epochs.local(cpu);
    if cpu == 0 {
        for _ in 0..UPDATES {
            shared.pair.update(&mut smr, |&(first, _)| (first + 1, 2 * (first + 1)));
        }
        tally(smr.counts());
        return;
    }
    let (mut reads, mut last) = (0, 0);
    while last < UPDATES {
        let (first, second) = shared.pair.read(&mut smr, |&pair| pair);
        if second != 2 * first || first < last {
            WRONG.fetch_add(1, Ordering::Relaxed);
        }
        last = first;
        reads += 1;
    }
    READS.fetch_add(reads, Ordering::Relaxed);
}

fn replaced() -> Result<(), &'static str> {
    READS.store(0, Ordering::Relaxed);
    let (orphans, took) = with_shared(0, |_| (), replaced_work, |shared, took| (shared.epochs.free_orphans(), took));
    let counts = tallied();
    println!("CPU 0 replaces a pair of numbers {UPDATES} times, by copying it, while the other three read it:");
    println!(
        "  every read saw one whole pair, none older than the read before; {} old pairs were retired, and {} freed",
        counts.retired,
        counts.freed + orphans
    );
    println!("  this run: {} reads; {took} ms", READS.load(Ordering::Relaxed));
    if WRONG.load(Ordering::Relaxed) != 0 {
        return Err("a reader saw half of one pair and half of another, or an older pair after a newer");
    }
    if counts.retired != UPDATES || counts.freed + orphans != counts.retired {
        return Err("not every pair that was replaced was retired and freed, once");
    }
    Ok(())
}

// ---- A queue of jobs ------------------------------------------------------------------

/// Count the primes from `from` up to `to`.
#[derive(Clone, Copy)]
struct Job {
    from: u32,
    to: u32,
}

/// Jobs that are in the queue, or being run.
static PENDING: AtomicUsize = AtomicUsize::new(0);
static RAN: [AtomicUsize; CPUS] = [const { AtomicUsize::new(0) }; CPUS];
static SPLIT: AtomicUsize = AtomicUsize::new(0);

fn primes(from: u32, to: u32) -> u32 {
    let prime = |n: u32| n >= 2 && (2..).take_while(|d| d * d <= n).all(|d| !n.is_multiple_of(d));
    (from..to).filter(|&n| prime(n)).count() as u32
}

/// One CPU's work on the job queue: take a job; if it is large, put its two halves in
/// the queue; if not, do it, and push the result. Stop when no job is queued or running.
fn job_work(cpu: usize) {
    let shared = shared();
    let mut smr = shared.epochs.local(cpu);
    while PENDING.load(Ordering::Acquire) != 0 {
        let Some(Job { from, to }) = shared.jobs.dequeue(&mut smr) else {
            spin_loop();
            continue;
        };
        if to - from > SMALL {
            let middle = from + (to - from) / 2;
            PENDING.fetch_add(2, Ordering::AcqRel);
            shared.jobs.enqueue(&mut smr, Job { from, to: middle });
            shared.jobs.enqueue(&mut smr, Job { from: middle, to });
            SPLIT.fetch_add(1, Ordering::Relaxed);
        } else {
            shared.results.push(primes(from, to) as usize);
        }
        RAN[cpu].fetch_add(1, Ordering::Relaxed);
        PENDING.fetch_sub(1, Ordering::AcqRel);
    }
    tally(smr.counts());
}

/// Count the primes below `LIMIT` on one CPU, and then as jobs on a queue that all four
/// CPUs take from and add to.
fn jobs() -> Result<u32, &'static str> {
    let start = Board::ticks();
    let alone = primes(0, LIMIT);
    let alone_took = ms(Board::ticks() - start);
    for count in RAN.iter().chain([&SPLIT]) {
        count.store(0, Ordering::Relaxed);
    }
    PENDING.store(1, Ordering::Release);
    let (found, results, orphans, took) = with_shared(
        0,
        |shared| shared.jobs.enqueue(&mut AtOnce::default(), Job { from: 0, to: LIMIT }),
        job_work,
        |shared, took| {
            // The CPUs are done, so this one is alone, and may free at once.
            let (mut found, mut results) = (0, 0);
            let mut at_once = AtOnce::default();
            while let Some(count) = pop_at_once(&shared.results, &mut at_once) {
                found += count as u32;
                results += 1;
            }
            (found, results, shared.epochs.free_orphans(), took)
        },
    );
    let counts = tallied();
    let ran = [0, 1, 2, 3].map(|cpu| RAN[cpu].load(Ordering::Relaxed));
    let split = SPLIT.load(Ordering::Relaxed);
    println!("a queue holds one job, to count the primes below {LIMIT}. each CPU takes a job, and splits it in two");
    println!("if it covers more than {SMALL} numbers:");
    println!(
        "  {} jobs ran: {split} were split, and {results} counted {found} primes; {} nodes were retired, and {} freed",
        ran.iter().sum::<usize>(),
        counts.retired,
        counts.freed + orphans
    );
    println!(
        "  this run: the CPUs ran {}, {}, {} and {} jobs; {took} ms, and one CPU counting alone took {alone_took} ms",
        ran[0], ran[1], ran[2], ran[3]
    );
    if found != alone {
        return Err("the jobs counted a different number of primes than one CPU did alone");
    }
    if ran.iter().sum::<usize>() != split + results || counts.retired != split + results {
        return Err("a job was lost, or run twice");
    }
    if counts.freed + orphans != counts.retired {
        return Err("a job's node was retired and never freed");
    }
    Ok(found)
}
