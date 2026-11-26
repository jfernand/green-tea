//! Link with green-tea-bare's memory layout (see green-tea-bare/build.rs).

fn main() {
    if let Ok(script) = std::env::var("DEP_GREEN_TEA_BARE_LINKER_SCRIPT") {
        println!("cargo::rustc-link-arg-bins=-T{script}");
    }
}