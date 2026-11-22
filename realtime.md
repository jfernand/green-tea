# A realtime kernel for green-tea: draft plan

A realtime kernel is judged by its worst case. An ordinary kernel tries to be fast on
average. A realtime kernel promises that when something becomes due, the right task is
running within a known time, every time. The number we care about is **latency**: how
late a task starts after the moment it should have started.

## What we have

- Tasks that can be preempted by a timer (`green-tea-preempt`).
- Allocators whose time per operation is bounded: TLSF, pools and slabs.
- No background services, so nothing runs that we didn't start.
- Starting several CPUs, with their APIC ids from ACPI.

## What stands in the way

| Today | Why it breaks a realtime promise |
|---|---|
| Round-robin scheduling (`runtime.rs`) | No priorities: an urgent task waits its turn behind every other task |
| Sleepers in an unsorted `Vec`, scanned on every switch | The cost of a switch grows with the number of sleeping tasks |
| The run queue is a `VecDeque`, and sleeping pushes on a `Vec` | Either can allocate during a switch |
| Every switch goes through a scheduler task, with interrupts off | Adds a second context switch, and the preempt demo draws on the screen in that window |
| The timer is the only interrupt | Devices are polled, so a task can't be woken by a device |
| Polled drivers can wait for a long time | USB recovery can block for seconds, and NVMe for up to 10 s per command |
| Heap pages are mapped on first touch | A page fault can arrive in the middle of urgent work |
| Spinlocks with no priority awareness | A low-priority task holding a lock can stall a high-priority one indefinitely (priority inversion) |
| One global scheduler | Other CPUs can't be scheduled independently |
| The APIC timer counts down, converted from TSC ticks | Adds rounding error to every deadline |

## What this laptop gives us

Read from Linux on this machine (i7-12700H):

- **TSC-deadline timer: present.** The CPU can interrupt at an exact TSC value, with no
  conversion. The TSC also runs at a constant rate and doesn't stop in sleep states.
- **Two kinds of core.** 6 fast cores (12 threads) and 8 efficient cores. Timing differs
  between them, so realtime work should stay on one kind.
- **Sleep states cost time.** Waking from the deepest takes about 230 µs, against 2 µs from
  the shallowest. A realtime CPU has to stay out of the deep ones.
- **System Management Interrupts (SMIs).** The firmware can take the CPU away at any moment,
  invisibly to any kernel, and we can't turn that off. So on this laptop we can measure
  a worst case, but **can't guarantee one**. This is the honest limit of the project.

## The design I recommend

- **Fixed priorities, preemptive.** The highest-priority ready task always runs. It is the
  simplest policy to reason about, and the standard analysis for it is well known.
- **Nothing allocates, and nothing scans, in the scheduler.** Tasks and their stacks are
  created up front. A bitmap of priorities finds the next task in constant time.
- **A new crate, `green-tea-rt`, with its logic testable on the host,** and a new kernel
  on top of it. The existing kernels keep working, and stay what the book teaches from.
- **One fast core dedicated to realtime,** with interrupts from everything else routed
  elsewhere, and no task migration.

## Phases

Each phase ends with something we can run and check.

0. **Measure first.** A `latency` kernel asks for a timer tick thousands of times per second
   and records how late each one is, as a histogram. QEMU can prove the tool works, but
   its timings mean nothing: the real numbers come from booting it on the laptop.
1. **An exact timer.** Use TSC-deadline mode, falling back to the APIC countdown, and check
   that the TSC is invariant (an open defect in Appendix C). Pass: phase 0's histogram
   gets narrower.
2. **The scheduler.** Priorities, constant-time selection, sorted timers, direct task-to-task
   switches. Pass: host tests, and a high-priority task preempts a busy one in QEMU.
3. **Locks.** A mutex with priority inheritance, and a measure of the longest time interrupts
   stay off. Pass: the classic priority-inversion scenario no longer stalls.
4. **Device interrupts.** IO APIC and MSI, with short handlers that wake a task. This is
   the subject of our next conversation. Pass: a key press wakes a task, with no polling.
5. **Memory.** Map and touch everything a realtime task uses before it starts. Pass: no page
   fault occurs in a realtime task, counted by the fault handler.
6. **Isolation.** Pin realtime work to one core, keep it out of deep sleep, and send other
   interrupts elsewhere. Pass: a narrower histogram under load on the other cores.
7. **Guarantees, as far as they go.** Declare each task's period and cost, refuse a task set
   that can't meet its deadlines, and detect and report any missed deadline.

## Decisions for you

1. **What is it for?** A teaching kernel for the book, or something driving real hardware?
   I assume teaching, with honest measurements.
2. **What latency target?** I suggest setting it after phase 0, from real numbers.
3. **Fixed priorities or earliest-deadline-first?** I recommend fixed priorities now. EDF can
   come later as a second policy, and comparing the two would make a good chapter.
