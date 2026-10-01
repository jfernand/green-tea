use std::arch::global_asm;

global_asm!(include_str!("riscv64.s"));

/// The registers `swap_context` saves and restores: `sp` and `ra`, plus the LP64D
/// callee-saved registers s0–s11 and fs0–fs11. Everything else is caller-saved, so the
/// compiler has already spilled anything it cares about before calling `swap_context`.
///
/// Saving fs0–fs11 assumes a hard-float (LP64D) target such as riscv64gc. riscv64.s turns
/// on the D extension for itself with `.option arch, +d`.
#[derive(Debug, Default)]
#[repr(C)]
pub struct Context {
    sp: u64,           // 0(a0)    stack pointer
    ra: u64,           // 8(a0)    return address: where execution resumes
    s0: u64,           // 16(a0)   frame pointer
    s1: u64,           // 24(a0)   callee saved; on a fresh context, the entry argument
    s2: u64,           // 32(a0)   callee saved; on a fresh context, the entry function
    s3_s11: [u64; 9],  // 40(a0)..104(a0)
    fs0_fs11: [u64; 12], // 112(a0)..200(a0) callee-saved floating point
}

const _: () = assert!(size_of::<Context>() == 208); // the offsets riscv64.s uses

unsafe extern "C" {
    /// Save the current registers into `from`, load `to`'s, and `ret` to `to`'s `ra`.
    pub fn swap_context(from: *mut Context, to: *const Context);
    fn bootstrap_entry() -> !; // never returns, runs unimp
}

/// A function a fresh context starts in. It must never return: there is nothing to return to.
pub type Entry = extern "C" fn(arg: *mut ()) -> !;

impl Context {
    /// A context that, when first switched to, calls `entry(arg)` on the stack ending at `top`.
    ///
    /// # Safety
    /// `top` must be the upper end of a writable stack, which must outlive every switch
    /// into the returned context.
    pub unsafe fn new(top: *mut u8, entry: Entry, arg: *mut ()) -> Context {
        // As on aarch64 nothing is pushed: `ret` jumps to ra, so the first switch "returns"
        // into `bootstrap_entry`. The psABI wants sp 16-byte aligned.
        let top = (top as usize) & !0xF;
        Context {
            sp: top as u64,
            ra: bootstrap_entry as *const () as u64,
            s1: arg as u64,
            s2: entry as *const () as u64,
            ..Context::default() // s0 = 0 ends frame-pointer backtraces here
        }
    }
}
