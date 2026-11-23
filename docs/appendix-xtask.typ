#import "lib.typ": *

= Every xtask command <app-xtask>

`cargo xtask` is an alias, set in `.cargo/config.toml`, for running the `xtask` crate. It has four commands, and @crate-xtask describes how each works.

#dtable(columns: (auto, 1fr), ("Command", "What it does"),
  [`test`], [Runs `green-tea-v2`'s tests and demos on the host and, under `qemu-user`, on aarch64 and riscv64, in both profiles. Builds the library for `aarch64-apple-darwin`, which can be compiled here but not run],
  [`kernel`], [Builds the kernels, for QEMU's machines or with `--board pc` for a physical PC, boots each in QEMU everywhere it runs, and judges each by its serial output],
  [`shell`], [Boots `green-tea-shell` with its serial console on this terminal. `halt` stops the kernel, and ctrl-a x leaves QEMU],
  [`trace`], [Boots the shell without a window, types at it, and saves the trace it dumps to a file, with the serial log beside it],
)

== Options

#dtable(columns: (auto, auto, 1fr), ("Command", "Option", "Meaning"),
  [`test`], [`--target <triple>`], [Only this target, which may be repeated. `host`, `aarch64-unknown-linux-gnu`, `riscv64gc-unknown-linux-gnu` or `aarch64-apple-darwin`. Default: all],
  [`test`], [`--profile <p>`], [Only `debug` or `release`, which may be repeated. Default: both],
  [`kernel`], [`--kernel <name>`], [Only this kernel, which may be repeated (@app-kernels). Default: all],
  [`kernel`], [`--target <arch>`], [Only `riscv64`, `aarch64` or `x86_64`, which may be repeated. Default: all three],
  [`kernel`], [`--release`], [Build with optimizations],
  [`kernel`], [`--headless`], [No windows: quit each QEMU as soon as its kernel finishes],
  [`kernel`], [`--screenshot`], [Save the final screen to `target/kernel/<kernel>-<arch>.png`],
  [`kernel`], [`--jobs <n>`], [At most this many virtual CPUs running at once. Default: half of the host's when headless, and no limit with windows],
  [`kernel`], [`--board <b>`], [`qemu`, the default, or `pc`: build for a physical x86_64 PC and test it in QEMU under UEFI. Runs on x86_64 only, with `disk` on NVMe],
  [`shell`], [`--target <arch>`], [The architecture. Default: riscv64],
  [`shell`], [`--release`], [Build with optimizations],
  [`trace`], [`--target <arch>`], [The architecture. Default: riscv64],
  [`trace`], [`--release`], [Build with optimizations],
  [`trace`], [`--out <file>`], [Where to write the trace. Default: `target/kernel/shell-<arch>.gtt`],
  [`trace`], [`--type <command>`], [A command to record in place of `session`, which may be repeated],
)

== The pc board

With `--board pc`, `kernel` makes a UEFI boot directory for each job, `target/kernel/pc/<kernel>-x86_64`, whose `EFI/BOOT/BOOTX64.EFI` is a GRUB built by `grub-mkstandalone` with its configuration and the kernel inside it. Copying that `EFI` directory to a FAT-formatted USB stick boots the kernel on a machine with Secure Boot off. The command needs `grub-mkstandalone` and the `ovmf` package.

Its QEMU run differs from a default one in five ways: OVMF is the firmware, with a private copy of its variables; the boot directory is a read-only FAT drive; the machine has 512 MiB and a standard VGA; an xHCI controller with a USB keyboard is attached; and serial input goes through a socket, sent after the kernel's banner, since the firmware and GRUB read the serial port too.

== The environment

`GREEN_TEA_MAX_CPUS` sets `cpus::MAX`, the most CPUs a kernel will start, at build time: a number from 1 to 256, and 8 if it isn't set. `green-tea-bare`'s build script checks it.
// chapter-status:begin
// stage: early-draft
// revision: 3
// body: ff91049841ea
// activity: 2026-11-22 | r3 | stage | green-tea | note | tracking started at early-draft
// chapter-status:end
