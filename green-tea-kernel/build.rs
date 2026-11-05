//! Fills in linker.ld's load address for the target architecture: where QEMU (or OpenSBI)
//! expects the kernel to be.

use std::{env, fs, path::PathBuf};

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").unwrap() != "none" {
        return; // a host build (e.g. clippy): the kernel only links for bare-metal targets
    }
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let base = match arch.as_str() {
        "riscv64" => "0x80200000", // where OpenSBI jumps, 2 MiB into RAM
        "aarch64" => "0x40200000", // 2 MiB into RAM, leaving QEMU room for the device tree
        "x86_64" => "0x100000",    // 1 MiB, above the legacy BIOS area
        other => panic!("green-tea-kernel has no platform for {other}"),
    };
    let template = fs::read_to_string("linker.ld").unwrap();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("linker.ld");
    fs::write(&out, template.replace("@BASE@", base)).unwrap();
    println!("cargo:rustc-link-arg-bins=-T{}", out.display());
    println!("cargo:rerun-if-changed=linker.ld");
}
