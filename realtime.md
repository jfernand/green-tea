# A realtime kernel for green-tea: draft plan

A realtime kernel is judged by its worst case. An ordinary kernel tries to be fast on
average. A realtime kernel promises that when something becomes due, the right task is
running within a known time, every time. The number we care about is **latency**: how
late a task starts after the moment it should have started.

**The kernel has to be real.** A phase passes only when it runs on the physical laptop.
QEMU and host tests catch regressions and prove that a tool works, but they don't finish a
phase, and QEMU's timings are never reported as results.

## What we have

- Tasks that can be preempted by a timer (`green-tea-preempt`).
- Allocators whose time per operation is bounded: TLSF, pools and slabs.
- No background services, so nothing runs that we didn't start.
- Starting several CPUs, with their APIC ids from ACPI.
- Keyboard, mouse and serial interrupts through the IO APIC on the `pc` board. They work in
  QEMU, and the keyboard's interrupt has worked on the laptop. The serial port's hasn't run
  there, and the mouse's hasn't been exercised anywhere.

## What stands in the way

| Today | Why it breaks a realtime promise |
|---|---|
| Round-robin scheduling (`runtime.rs`) | No priorities: an urgent task waits its turn behind every other task |
| Sleepers in an unsorted `Vec`, scanned on every switch | The cost of a switch grows with the number of sleeping tasks |
| The run queue is a `VecDeque`, and sleeping pushes on a `Vec` | Either can allocate during a switch |
| Every switch goes through a scheduler task, with interrupts off | Adds a second context switch, and the preempt demo draws on the screen in that window |
| NVMe, xHCI and virtio are polled (no MSI or MSI-X) | A task can't be woken by those devices |
| Device handlers run only while the CPU is idle | A device can't preempt a running task |
| If one task sleeps while another waits for a device, the scheduler spins with interrupts off | The waiter can't wake until the sleeper is due |
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
  and we can't turn that off. So on this laptop we can measure a worst case, but **can't
  guarantee one**. This is the honest limit of the project. The kernel should detect and
  report this lost time, from the CPU's SMI counter and from gaps in the TSC, and any worst
  case we publish is "measured on this laptop, including SMIs".

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

Each phase ends with something we can run and check, and every pass below is on the laptop.
The shell has booted on the laptop and taken keyboard interrupts there, so the boot path
and timer setup that the `latency` kernel depends on work on the real machine.

0. **Measure first.** A `latency` kernel asks for a timer tick thousands of times per second
   and records how late each one is, as a histogram, along with the SMIs it saw. QEMU can
   prove the tool works, but its timings mean nothing. Pass: a histogram from the laptop.
1. **An exact timer.** Use TSC-deadline mode, falling back to the APIC countdown, and check
   that the TSC is invariant (an open defect in Appendix C). Pass: phase 0's histogram
   gets narrower.
2. **The scheduler.** Priorities, constant-time selection, sorted timers, direct task-to-task
   switches, and no spinning with interrupts off while a task sleeps. Pass: a high-priority
   task preempts a busy one, and a task waiting for a key wakes while another sleeps.
3. **Locks.** A mutex with priority inheritance, and a measure of the longest time interrupts
   stay off. Pass: the classic priority-inversion scenario no longer stalls.
4. **Device interrupts.** Partly written: the IO APIC, with handlers that wake a waiting
   task while the CPU is idle. Still to do: MSI or MSI-X for NVMe and xHCI, and handlers
   that can preempt a running task. Pass: a key press wakes a task, with no polling.
5. **Memory.** Map and touch everything a realtime task uses before it starts. Pass: no page
   fault occurs in a realtime task, counted by the fault handler.
6. **Isolation.** Pin realtime work to one core, keep it out of deep sleep, and send other
   interrupts elsewhere. Pass: a narrower histogram under load on the other cores.
7. **Guarantees, as far as they go.** Declare each task's period and cost, refuse a task set
   that can't meet its deadlines, and detect and report any missed deadline.

## Decisions for you

1. **What latency target?** I suggest setting it after phase 0, from real numbers.
2. **Fixed priorities or earliest-deadline-first?** I recommend fixed priorities now. EDF can
   come later as a second policy, and comparing the two would make a good chapter.

Decided: the kernel runs on real hardware, even where it is used for teaching.
