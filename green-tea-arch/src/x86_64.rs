use core::arch::global_asm;

// x86_64-unknown-none is soft-float: SSE is off in CR4 there, so `stmxcsr` would fault, and
// no code uses the floating-point state anyway. x86_64.s skips it unless the target has SSE.
global_asm!(
    ".set GREEN_TEA_FP_CONTROL, {fp_control}",
    include_str!("x86_64.s"),
    fp_control = const cfg!(target_feature = "sse2") as u8,
);

/// MXCSR at reset: round to nearest, every exception masked.
const DEFAULT_MXCSR: u32 = 0x1F80;
/// The x87 control word after `fninit`: round to nearest, extended precision, all masked.
const DEFAULT_X87_CW: u16 = 0x037F;

/// The registers `swap_context` saves and restores: `rsp` plus the SysV callee-saved
/// registers, and the floating-point control state (MXCSR and the x87 control word), which
/// the ABI also makes callee-saved. Everything else is caller-saved, so the compiler has
/// already spilled anything it cares about before calling `swap_context`.
#[derive(Debug, Default)]
#[repr(C)]
pub struct Context {
    rsp: u64, // [rdi + 0x00] stack pointer: where execution resumes
    r15: u64, // [rdi + 0x08] callee saved; on a fresh context, the entry argument
    r14: u64, // [rdi + 0x10] callee saved; on a fresh context, the entry function
    r13: u64, // [rdi + 0x18]
    r12: u64, // [rdi + 0x20]
    rbx: u64, // [rdi + 0x28]
    rbp: u64, // [rdi + 0x30]
    mxcsr: u32,   // [rdi + 0x38] SSE control and status; not switched on soft-float targets
    x87_cw: u16,  // [rdi + 0x3C] x87 control word; likewise
}

// The offsets x86_64.s uses.
const _: () = assert!(size_of::<Context>() == 64);
const _: () = assert!(core::mem::offset_of!(Context, mxcsr) == 0x38);
const _: () = assert!(core::mem::offset_of!(Context, x87_cw) == 0x3C);

unsafe extern "C" {
    /// Save the current registers into `from`, load `to`'s, and `ret` onto `to`'s stack.
    pub fn swap_context(from: *mut Context, to: *const Context);
    fn bootstrap_entry() -> !; // never returns, runs ud2 instruction
}

/// A function a fresh context starts in. It must never return: there is nothing to return to.
pub type Entry = extern "C" fn(arg: *mut ()) -> !;

impl Context {
    /// A context that, when first switched to, calls `entry(arg)` on the stack ending at `top`.
    ///
    /// # Safety
    /// `top` must be the upper end of a writable stack with room for at least a few words,
    /// which must outlive every switch into the returned context.
    pub unsafe fn new(top: *mut u8, entry: Entry, arg: *mut ()) -> Context {
        // The slot `rsp` points to holds the address the first `swap_context` will `ret`
        // into (`bootstrap_entry`). It must sit at an address ≡ 8 (mod 16): after `ret`
        // pops it, rsp ≡ 0, and `call r14` then pushes to ≡ 8, which is what the SysV ABI
        // expects at function entry. `bootstrap_entry` also does `and rsp, -16` before the
        // call, so this is belt and braces.
        let top = (top as usize) & !0xF;
        let sp = (top - 24) as *mut u64;
        unsafe { sp.write(bootstrap_entry as *const () as u64) };
        Context {
            rsp: sp as u64,
            r15: arg as u64,
            r14: entry as *const () as u64,
            mxcsr: DEFAULT_MXCSR,
            x87_cw: DEFAULT_X87_CW,
            ..Context::default()
        }
    }
}
