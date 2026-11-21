#import "lib.typ": *

= Every xtask command <app-xtask>

`cargo xtask` is an alias, set in `.cargo/config.toml`, for running the `xtask` crate. It has four commands, and @crate-xtask describes how each works.

#dtable(columns: (auto, 1fr), ("Command", "What it does"),
  [`test`], [Runs `green-tea-v2`'s tests and demos on the host and, under `qemu-user`, on aarch64 and riscv64, in both profiles. Builds the library for `aarch64-apple-darwin`, which can be compiled here but not run],
  [`kernel`], [Builds the kernels, boots each in QEMU everywhere it runs, and judges each by its serial output],
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
  [`shell`], [`--target <arch>`], [The architecture. Default: riscv64],
  [`shell`], [`--release`], [Build with optimizations],
  [`trace`], [`--target <arch>`], [The architecture. Default: riscv64],
  [`trace`], [`--release`], [Build with optimizations],
  [`trace`], [`--out <file>`], [Where to write the trace. Default: `target/kernel/shell-<arch>.gtt`],
  [`trace`], [`--type <command>`], [A command to record in place of `session`, which may be repeated],
)
