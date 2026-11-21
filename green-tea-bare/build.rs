//! Fills in linker.ld's load address for the target architecture (where QEMU, or OpenSBI,
//! expects the kernel to be) and tells the kernels where the result is.
//!
//! A library's build script can't add linker arguments to its dependents' binaries, so
//! each kernel's build.rs passes `-T` itself, with the path from
//! DEP_GREEN_TEA_BARE_LINKER_SCRIPT (see `links` in Cargo.toml).

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo::rerun-if-changed=linker.ld");
    if env::var("CARGO_CFG_TARGET_OS").unwrap() != "none" {
        return; // not a bare-metal target: nothing will link
    }
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let pc = env::var_os("CARGO_FEATURE_BOARD_PC").is_some();
    let base = match arch.as_str() {
        "riscv64" => "0x80200000", // where OpenSBI jumps, 2 MiB into RAM
        "aarch64" => "0x40200000", // 2 MiB into RAM, leaving QEMU room for the device tree
        // UEFI firmware keeps data of its own a few MiB up, where the kernel would overlap it.
        "x86_64" if pc => "0x2000000", // 32 MiB
        "x86_64" => "0x100000",    // 1 MiB, above the legacy BIOS area
        other => panic!("green-tea-bare has no platform for {other}"),
    };
    let template = fs::read_to_string("linker.ld").unwrap();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("linker.ld");
    fs::write(&out, template.replace("@BASE@", base)).unwrap();
    println!("cargo::metadata=linker_script={}", out.display());
}
