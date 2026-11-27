#import "../lib.typ": *

= green-tea-black <crate-black>

`green-tea-black` is the first kernel built on the Black profile (@crate-capabilities): it runs small programs as *user processes*, in ring 3 of an `x86_64` CPU, each with page tables of its own, and checks that a process can leave only by asking, can't touch the kernel, the I/O ports or another process, and that a process that faults is cleaned up while the kernel carries on. On the `pc` board it also lends the PS/2 keyboard to a process, through the kernel, and takes it back. The kernel is the test; the mechanism is in `green-tea-bare`, which this chapter describes as well.

#spec(
  ("Source", [#repo("kernels/black/src/main.rs"), 197 lines, and a `build.rs` that links with `green-tea-bare`'s memory layout. The mechanism: #repo("green-tea-bare/src/platform/x86_64/user.rs"), 298 lines, with the syscall entry, segments and page tables in `x86_64/mod.rs` and the keyboard broker in `x86_64/pc.rs`]),
  ("Depends on", [`green-tea-bare` and @crate-scheduler-coop, both with `black` instead of their default `green`; `board-pc` passes through to both]),
  ("Used by", [nothing; it is a kernel]),
  ("Tests", [it is one: `cargo xtask kernel --kernel protected`, on `x86_64`, on QEMU's `q35` and on the `pc` board in QEMU]),
)

`green-tea-bare` exports the user-process API on `x86_64` only, and the keyboard lease only with `board-pc`:

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`UserProcess::new(code, user_base)`], [a process whose code page holds `code` (at most 4 KiB) at virtual address `user_base`, with a stack page above it],
  [`UserProcess::with_i8042(code, user_base, &lease)`], [the same, allowed the scan-code syscall, and borrowing the lease for as long as it lives (`board-pc`)],
  [`run()`], [enter the process in ring 3, and return when it exits or faults],
  [`code_address()`], [the user address of its code page],
  [`ProcessOutcome`], [`Exited(status)` or `Fault { vector, error, address }`],
  [`ProcessError`], [`EmptyProgram`, `ProgramTooLarge`, `InvalidUserRegion`, `UnsupportedCapability`, `NoKernelStack`],
  [`USER_REGION_A`, `USER_REGION_B`], [two user addresses, `0x7fff_ffff_d000` and `0x7fff_ffff_9000`, for two processes that must not see each other],
  [`Pc::claim_user_i8042()`], [move the PS/2 controller from the kernel to a broker for user processes; returns a `UserI8042Lease`],
  [`UserI8042Lease`], [while it lives, keyboard bytes go to a 32-byte queue for processes; `dropped_scan_codes()` counts what didn't fit; dropping it gives the keyboard back],
  [`Platform::user_i8042_byte()`], [one byte from that queue; the default, for every other board, is an error],
)

The kernel's two syscalls go through `int 0x80`, with the number in `rax` and the argument in `rdi`, and the answer in `rax`:

#dtable(columns: (auto, auto, 1fr), ("`rax`", "Name", "What it does"),
  [1], [exit], [ends the process with status `rdi`, which must fit in 32 bits; it doesn't return],
  [2], [get scan code], [waits up to `rdi` milliseconds, at most 5,000, for a keyboard byte, and returns it; only for a process made by `with_i8042`],
  [other], [], [returns `u64::MAX` in `rax`, as does a refused or timed-out call],
)

== Segments and the syscall gate

On `x86_64` a privilege level is the low two bits of the code segment selector in `cs`: 0 for the kernel, 3 for user code. `green-tea-bare`'s GDT gains two descriptors, user data at index 3 and user code at index 4, which with the requested privilege level make the selectors `0x1b` and `0x23`. Each CPU's TSS moves from `0x18` to `0x28` to make room:

#srcfile("green-tea-bare/src/platform/x86_64/mod.rs", lines: ((403, 410),), notes: ((407, [`f2`: present, privilege 3, writable data.]), (408, [`fa`: present, privilege 3, code; the `a` in the next byte sets the 64-bit flag.]),))[```rust
static mut GDT: [u64; GDT_ENTRIES] = {
    let mut gdt = [0; GDT_ENTRIES];
    gdt[1] = 0x00af9a000000ffff;
    gdt[2] = 0x00cf92000000ffff;
    gdt[3] = 0x00cff2000000ffff;
    gdt[4] = 0x00affa000000ffff;
    gdt
};
```]

Data below code at `0x18` and `0x20` is the layout the `sysret` instruction would need, though the kernel enters and leaves with interrupts and `iretq` only.

An interrupt gate has a privilege level too, which says who may raise it with `int`. Every existing gate has 0 (`0x8e`), so a user `int` on any of them faults. Vector `0x80` gets a gate of its own with level 3 (`0xee`), and is the only door. When a ring-3 program takes it, the CPU switches to the stack in the TSS's `rsp0` and pushes the user's `ss`, `rsp`, `rflags`, `cs` and `rip`. The entry then pushes a zero error code, the vector 128, and all fifteen general registers, which is the `ExceptionFrame` layout the exception handlers already use:

#srcfile("green-tea-bare/src/platform/x86_64/mod.rs", lines: ((318, 334),), notes: ((319, [The error code an exception would have pushed, and then the vector, so the frame matches.]), (325, [The frame's address, as the first argument.]), (327, [Align the stack for the call, keeping the old value in `rbx`, which the call preserves.]), (330, [Restore every register, including `rax`, which the handler may have overwritten with the answer.]),))[```asm
green_tea_syscall_entry:
    push $0
    push $128
    .irp r, rax,rbx,rcx,rdx,rsi,rdi,rbp,r8,r9,r10,r11,r12,r13,r14,r15
    push %\r
    .endr
    cld
    mov %rsp, %rdi
    mov %rsp, %rbx
    and $-16, %rsp
    call green_tea_syscall
    mov %rbx, %rsp
    .irp r, r15,r14,r13,r12,r11,r10,r9,r8,rbp,rdi,rsi,rdx,rcx,rbx,rax
    pop %\r
    .endr
    add $16, %rsp
    iretq
```]

`green_tea_syscall` asserts that the vector is `0x80` and that the caller was in ring 3, and calls `user::syscall`. In the frame, the last register pushed comes first, so `_registers[14]` is `rax` and `_registers[9]` is `rdi`.

== An address space per process

`UserProcess::new` checks the board first: without `ProtectedAddressSpaces` among its capabilities it returns `UnsupportedCapability`, so a Green build can't make a process at all. Then it allocates two zeroed 4 KiB pages from the kernel heap, copies the program into the first, and builds the process's page tables:

#srcfile("green-tea-bare/src/platform/x86_64/user.rs", lines: ((107, 120),), notes: ((110, [The tables point at the pages' physical addresses. The heap is identity-mapped, so their kernel addresses are the physical ones.]), (116, [The kernel stack the CPU switches to on `int 0x80`, a 16 KiB slot from the same pool as the tasks' stacks, with its guard page.]), (119, [The stack page is the second; the stack grows down from the end of it.]),))[```rust
let mut code_page = zeroed_user_page();
code_page.0[..code.len()].copy_from_slice(code);
let stack_page = zeroed_user_page();
let address_space = paging::UserAddressSpace::new(
    user_base,
    &*code_page as *const UserPage as usize,
    &*stack_page as *const UserPage as usize,
)
.map_err(|_| ProcessError::InvalidUserRegion)?;
let kernel_stack = Stack::try_take().ok_or(ProcessError::NoKernelStack)?;
let entry = UserEntry {
    instruction: user_base,
    stack_top: user_base + PAGE * 2,
};
```]

`UserAddressSpace::new`, in `x86_64/mod.rs`, makes a new top-level table (PML4) and copies the boot table's 512 entries into it, so the kernel stays mapped exactly as before. None of those mappings has the *user* bit (bit 2), so ring 3 can't use them. It then fills one empty slot with three new tables, which map only the two pages:

#srcfile("green-tea-bare/src/platform/x86_64/mod.rs", lines: ((1040, 1044),), notes: ((1042, [The code page: present and user, but not writable, so the program can't change its own code.]), (1043, [The stack page: present, user and writable.]),))[```rust
pdpt.0[pdpt_index] = table_address(&pd) | PRESENT | WRITABLE | USER;
pd.0[pd_index] = table_address(&pt) | PRESENT | WRITABLE | USER;
pt.0[code_index] = code as u64 | PRESENT | USER;
pt.0[stack_index] = stack as u64 | PRESENT | WRITABLE | USER;
root.0[pml4_index] = table_address(&pdpt) | PRESENT | WRITABLE | USER;
```]

Before this it refuses a base that isn't page-aligned, two pages that don't both fall in PML4 slot 255 and in one page table, and a slot the boot table already uses. Slot 255 is the top of the lower half of the address space, which starts at `0x7f80_0000_0000`; the kernel lives in the first 4 GiB, in slot 0. With `USER_REGION_A`, the code page is `0x7fff_ffff_d000`, the stack page `0x7fff_ffff_e000`, and the stack starts at `0x7fff_ffff_f000`. Region B is four pages lower. Each process has its own top-level table and its own slot-255 subtree, so B's pages aren't in A's tables at all.

== Entering ring 3 and coming back

`run` is called from a kernel task, on the boot CPU. It turns interrupts off, records the process in `CURRENT`, and saves the kernel's page-table root and the TSS's `rsp0`. It points `rsp0` at the process's kernel stack, makes a `Context` (@crate-arch) that starts `enter_user` on that stack, loads the process's root into `cr3`, and switches:

#srcfile("green-tea-bare/src/platform/x86_64/user.rs", lines: ((167, 180),), notes: ((177, [The kernel task's registers go into `running.resume`. This call returns only when `finish` switches back to them.]), (178, [Back in the kernel's own tables.]),))[```rust
traps::set_kernel_stack(
    cpu,
    self.kernel_stack
        .top() as usize,
);
unsafe {
    paging::activate(
        self.address_space
            .root(),
    );
    swap_context(&mut running.resume, &user_context);
    paging::activate(old_root);
}
traps::set_kernel_stack(cpu, old_kernel_stack);
```]

`enter_user` loads the user data selector into `ds` and `es`, and builds by hand the frame an interrupt from ring 3 would have pushed, so that `iretq` "returns" to a place the program has never been:

#srcfile("green-tea-bare/src/platform/x86_64/user.rs", lines: ((224, 236),), notes: ((225, [`ss` and `rsp`: the user data segment and the top of the stack page.]), (227, [`rflags` = 2: bit 1 is always set, and everything else is clear, interrupts included.]), (228, [`cs` = `0x23`, ring 3, and `rip` = the first byte of the code page.]),))[```rust
asm!(
    "push {data}",
    "push {stack}",
    "push 2",
    "push {code}",
    "push {instruction}",
    "iretq",
    data = const USER_DATA_SELECTOR,
    code = const USER_CODE_SELECTOR,
    stack = in(reg) entry.stack_top as u64,
    instruction = in(reg) entry.instruction as u64,
    options(noreturn),
);
```]

The process leaves in one of two ways, and both end in `finish`. The exit syscall calls it with `Exited(status)`. Any exception taken in ring 3 reaches `green_tea_fault`, which now checks `cs` first, and for a user `cs` calls `user::fault` before the kernel's own fault handling, the demand paging of the heap included. `fault` records the vector, the error code, and for a page fault (vector 14) the address from `cr2`:

#srcfile("green-tea-bare/src/platform/x86_64/user.rs", lines: ((287, 298),), notes: ((293, [The outcome goes into the `Running` record on the kernel task's stack, which `run` is still waiting in.]), (295, [The registers saved here are never resumed. The user's kernel stack, or the exception stack, is abandoned where it stands.]),))[```rust
fn finish(outcome: ProcessOutcome) -> ! {
    let current = CURRENT.load(Ordering::Acquire);
    assert!(!current.is_null(), "user trap without an active process");
    // SAFETY: the boot CPU published this live stack record before entering ring 3 and
    // remains the only CPU that can reach the user trap path.
    unsafe {
        (*current).outcome = Some(outcome);
        let mut abandoned = Context::default();
        swap_context(&mut abandoned, &(*current).resume);
    }
    unreachable!("a terminated user process resumed")
}
```]

Back in `run`, the kernel's root and `rsp0` are restored, `ds` and `es` get the kernel data selector `0x10` again, `CURRENT` is cleared, interrupts come back on if they were on before, and the outcome is returned. Dropping the `UserProcess` frees its two pages and four tables to the heap, and its kernel stack to the pool.

== Mediated keyboard input

A user process can't read the keyboard's ports: `in` and `out` from ring 3 fault, since the I/O privilege level in `rflags` is 0 and the TSS grants no ports. On the `pc` board it can get scan codes from the kernel instead, if it was given them. `Pc::claim_user_i8042` moves the controller to a broker, before the kernel's console input has started: it switches the keyboard's owner to the user side, empties a 32-entry queue, and turns on the keyboard and mouse interrupts through the IO APIC (@crate-ioapic). From then on the interrupt handler reads each byte raw, with @crate-i8042's `read_raw`, and queues the keyboard's bytes undecoded, discarding the mouse's:

#srcfile("green-tea-bare/src/platform/x86_64/pc.rs", lines: ((310, 321),), notes: ((310, [The kernel's own `getc` checks the same owner, and skips the controller while a process has it.]), (315, [The byte must be read in the handler, whoever wants it: an unread byte blocks the controller's one output buffer.]), (317, [A full queue counts the byte in `USER_KEY_DROPS` and loses it.]),))[```rust
if KEYBOARD_OWNER.load(Ordering::Acquire) == USER_KEYBOARD_OWNER
    && matches!(vector, KEYBOARD_VECTOR | MOUSE_VECTOR)
{
    // SAFETY: the lease excludes kernel polling, this handler is the sole decoder
    // writer, and only the boot CPU receives these input routes.
    if let Some(input) = unsafe { (&raw mut KEYBOARD).as_mut() }.and_then(Keyboard::read_raw) {
        if !input.auxiliary {
            push_user_key(input.code);
        }
    }
    return;
}
```]

Two things must both hold for a process to get a byte. The process must have been made by `with_i8042`, which takes a reference to the `UserI8042Lease`: the process type carries the lease's lifetime, so the compiler won't let the lease be dropped while such a process exists. And the controller must still be the broker's, which `user_i8042_byte` checks. The syscall then loops, taking a byte if one is queued and halting until the next interrupt if not, until its deadline:

#srcfile("green-tea-bare/src/platform/x86_64/user.rs", lines: ((240, 256),), notes: ((244, [Not allowed, or a zero timeout: the answer is `u64::MAX` at once.]), (248, [The wait is capped at 5,000 ms, so a process can hold the kernel for at most five seconds per call.]), (256, [Interrupts are off in the syscall (it came through an interrupt gate). `wait_for_interrupt` turns them on for its `hlt` only, so the keyboard's interrupt can arrive and fill the queue.]),))[```rust
pub(super) fn syscall(frame: &mut ExceptionFrame) {
    let number = frame._registers[14];
    let argument = frame._registers[9];
    if number == GET_SCAN_CODE_SYSCALL {
        if !current_has_i8042() || argument == 0 {
            frame._registers[14] = u64::MAX;
            return;
        }
        let wait_ticks = Board::ticks_per_ms().saturating_mul(argument.min(MAX_SCAN_CODE_WAIT_MS));
        let deadline = Board::ticks().saturating_add(wait_ticks);
        loop {
            match Board::user_i8042_byte() {
                Ok(Some(byte)) => {
                    frame._registers[14] = u64::from(byte);
                    return;
                }
                Ok(None) if Board::ticks() < deadline => Board::wait_for_interrupt(),
```]

Dropping the lease gives the controller back to the kernel and discards whatever is still queued. The kernel's `getc` reads it again, decoding as before.

== The kernel's checks

The programs are machine code written into byte arrays, as there is no user-space toolchain. The smallest is `exit_program`, fourteen bytes:

#srcfile("kernels/black/src/main.rs", lines: ((132, 145),), notes: ((134, [`b8 01 00 00 00` is `mov eax, 1`, the exit syscall's number.]), (135, [`bf` is `mov edi, imm32`: the status, little-endian.]), (142, [`cd 80` is `int 0x80`.]), (143, [`0f 0b` is `ud2`, an invalid instruction: if exit ever returned, the process would fault with vector 6 instead of running on.]),))[```rust
fn exit_program(code: u32) -> [u8; 14] {
    let mut program = [0u8; 14];
    program[..5].copy_from_slice(&[0xb8, 1, 0, 0, 0]); // mov eax, EXIT
    program[5..10].copy_from_slice(&[
        0xbf,
        code as u8,
        (code >> 8) as u8,
        (code >> 16) as u8,
        (code >> 24) as u8,
    ]);
    program[10..12].copy_from_slice(&[0xcd, 0x80]); // int 0x80
    program[12..14].copy_from_slice(&[0x0f, 0x0b]); // ud2 if exit returned
    program
}
```]

The others are built from it. `syscall_error_program(n)` makes syscall `n`, compares `rax` with `-1`, and exits 0 if the call was refused and 1 if not. `read_program(address)` loads eight bytes from `address` (`48 a1`, `mov rax, [moffs64]`) and exits 0. `port_program` writes to port `0x60`, the keyboard's data port. `get_key_program` asks for a scan code with a 3,000 ms timeout, retries on anything but `0x1e`, the make code of A in scan code set 1, and exits with status `'a'` (97).

`process_tests` runs as a task on @crate-scheduler-coop, and creates and runs each process in turn, asserting the outcome:

#dtable(columns: (auto, auto, 1fr), ("Check", "Expected", "Why"),
  [valid exit], [`Exited(42)`], [the exit syscall carries a status back],
  [syscall validation], [`Exited(0)`], [syscall 3 doesn't exist, and answers `-1`],
  [kernel mapping denial], [`Fault` 14, error 5, at the kernel's first address], [error bit 0: the page is present; bit 2: the access came from user mode. The kernel is mapped, and refused],
  [port I/O denial], [`Fault` 13], [`out` in ring 3 with I/O privilege level 0 is a general protection fault],
  [sibling mapping denial], [`Fault` 14, error 4, at B's code page], [bit 0 clear: in A's tables the page doesn't exist. B then runs and exits 0, unharmed],
)

On the `pc` board five more steps follow. The kernel claims the controller. A process made with `new`, without the lease, asks for a scan code and is refused. A process made with `with_i8042` waits for A, which `cargo xtask` types at QEMU's emulated PS/2 keyboard with the monitor's `sendkey a` when it sees the line `user keyboard: waiting for A scan code`. A process with the lease that writes to port `0x60` still faults: the lease is a right to the kernel's queue, not to the hardware. Then the kernel drops the lease and waits for B at its own `getc`, which `xtask` types the same way.

A second task, the *witness*, sets a flag when it runs. `kmain` asserts the flag after the scheduler has run both tasks, so the kernel prints `PASS` only if ordinary tasks still ran after five contained faults.

== Exercised by

On QEMU's `q35` the keyboard steps are compiled out:

#output(title: "cargo xtask kernel --kernel protected --target x86_64 --headless")[
```
green-tea-black on x86_64
…
user process: valid exit
user process: syscall validation
user process: kernel mapping denial
user process: port I/O denial
user process: sibling mapping denial
protected process checks completed; kernel task resumed after contained faults
kernel witness task continued after user faults
PASS: user exit, syscall checks, kernel/port/page isolation and fault cleanup
```
]

On the `pc` board in QEMU, with OVMF and GRUB, `xtask` attaches no USB keyboard for this kernel, and the Black build would refuse one anyway: its first line, before the banner, is the board declining to give the USB controller DMA. The `sending` lines are `xtask`'s, interleaved with the kernel's serial log:

#output(title: "cargo xtask kernel --board pc --kernel protected --target x86_64 --headless")[
```
[protected-x86_64] green-tea-black on x86_64 (pc)
…
[protected-x86_64] user process: sibling mapping denial
[protected-x86_64] user keyboard: controller lease acquired
[protected-x86_64] user keyboard: ungranted process denied
[protected-x86_64] user keyboard: waiting for A scan code
[protected-x86_64] sending PS/2 key A
[protected-x86_64] user keyboard: decoded A, dropped scan codes 0
[protected-x86_64] user driver fault contained; releasing controller lease
[protected-x86_64] kernel keyboard: waiting for B
[protected-x86_64] sending PS/2 key B
[protected-x86_64] kernel keyboard fallback delivered B after lease cleanup
…
[protected-x86_64] PASS: user exit, syscall checks, kernel/port/page isolation and fault cleanup
```
]

#output(title: "the serial log of the same run, after the firmware's lines")[
```
usb: refusing DMA-capable assignment without IOMMU enforcement
green-tea-black on x86_64 (pc)
framebuffer: 1280x800 XRGB8888
```
]

== Limits

- Run only in QEMU, on `q35` and on the `pc` board. It hasn't run on the laptop.
- A process runs with interrupts off (`rflags` = 2), so the timer can't take the CPU back: a program that loops without a syscall hangs the kernel. `run` is also synchronous, and the cooperative scheduler runs no other task until it returns.
- One process at a time, on the boot CPU only; `run` asserts both. There is no scheduler of processes, no `fork` or `exec`, no loader: a program is at most one 4 KiB page of machine code, with one 4 KiB stack page.
- Isolation rests on the user bit alone. Every process's tables contain the whole kernel mapping, supervisor-only, with no separate kernel tables (as Linux's page-table isolation has against Meltdown), and the code doesn't turn on SMEP or SMAP, which would stop the kernel from executing or touching user pages by mistake. No page is marked no-execute, so the stack page is executable.
- The process's pages are kernel heap pages, which stay writable through the kernel's identity map.
- The frame's registers are read by index, `_registers[14]` and `_registers[9]`; a change to the push order in the entry stub would silently swap them.
- `get_key_program` treats a byte of `0xff` as a refusal: the scan code `0xff`, which a keyboard sends on an error, and the sentinel `u64::MAX` look the same in `al`.
- The board claims `ProtectedAddressSpaces` because the build is Black (@crate-capabilities), not because a check found the mechanism working; this kernel is that check, run by hand.
- The Black profile has no DMA devices: `q35` refuses virtio over PCI, and `pc` refuses xHCI and NVMe, until an IOMMU backend exists (@crate-dma).

Two pages, four tables, one gate at `0x80`, and a kernel that outlives the programs it runs.
// chapter-status:begin
// stage: research-done
// revision: 1
// body: f71eed769a62
// activity: 2026-11-27 | r1 | stage | green-tea | note | tracking started at research-done
// chapter-status:end
