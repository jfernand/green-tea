use std::arch::global_asm;

global_asm!(include_str!("aarch64.s"));

/// The registers `swap_context` saves and restores: `sp` plus the AAPCS64 callee-saved
/// registers, including the low halves of v8–v15 (d8–d15). Everything else is
/// caller-saved, so the compiler has already spilled anything it cares about before
/// calling `swap_context`.
#[derive(Debug, Default)]
#[repr(C)]
pub struct Context {
    sp: u64,       // [x0, #0]   stack pointer
    x19: u64,      // [x0, #8]   callee saved; on a fresh context, the entry argument
    x20: u64,      // [x0, #16]
    x21: u64,      // [x0, #24]  callee saved; on a fresh context, the entry function
    x22: u64,      // [x0, #32]
    x23: u64,      // [x0, #40]
    x24: u64,      // [x0, #48]
    x25: u64,      // [x0, #56]
    x26: u64,      // [x0, #64]
    x27: u64,      // [x0, #72]
    x28: u64,      // [x0, #80]
    x29: u64,      // [x0, #88]  frame pointer
    x30: u64,      // [x0, #96]  link register: where execution resumes
    d8_d15: [u64; 8], // [x0, #104..#160] callee-saved floating point
}

const _: () = assert!(size_of::<Context>() == 168); // the offsets aarch64.s uses

unsafe extern "C" {
    /// Save the current registers into `from`, load `to`'s, and `ret` to `to`'s `x30`.
    pub fn swap_context(from: *mut Context, to: *const Context);
    fn bootstrap_entry() -> !; // never returns, runs brk
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
        // Unlike x86_64 there's nothing to push: `ret` jumps to x30, so the first switch
        // "returns" into `bootstrap_entry` without touching the stack. AAPCS64 wants sp
        // 16-byte aligned at all times, and the hardware faults on a misaligned sp.
        let top = (top as usize) & !0xF;
        Context {
            sp: top as u64,
            x19: arg as u64,
            x21: entry as *const () as u64,
            x30: bootstrap_entry as *const () as u64,
            ..Context::default() // x29 = 0 ends frame-pointer backtraces here
        }
    }
}
