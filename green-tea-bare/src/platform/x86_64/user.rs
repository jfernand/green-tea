use super::{ExceptionFrame, paging, traps};
use crate::stack::Stack;
use crate::{Board, Platform};
use alloc::alloc::{Layout, alloc_zeroed, handle_alloc_error};
use alloc::boxed::Box;
use core::arch::asm;
use core::marker::PhantomData;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicPtr, Ordering};
use green_tea_arch::{Context, swap_context};
use green_tea_capabilities::Capability;

const PAGE: usize = 4096;
const USER_DATA_SELECTOR: u16 = 0x1b;
const USER_CODE_SELECTOR: u16 = 0x23;
const EXIT_SYSCALL: u64 = 1;
const GET_SCAN_CODE_SYSCALL: u64 = 2;
const MAX_SCAN_CODE_WAIT_MS: u64 = 5_000;

/// Distinct two-page user regions used by the protected-process demonstration.
pub const USER_REGION_A: usize = 0x0000_7fff_ffff_d000;
pub const USER_REGION_B: usize = 0x0000_7fff_ffff_9000;

#[repr(C, align(4096))]
struct UserPage([u8; PAGE]);

struct UserEntry {
    instruction: usize,
    stack_top: usize,
}

struct Running {
    resume: Context,
    outcome: Option<ProcessOutcome>,
    allow_i8042: bool,
}

static CURRENT: AtomicPtr<Running> = AtomicPtr::new(null_mut());

/// The result of returning to the kernel from a user process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessOutcome {
    Exited(u32),
    Fault {
        vector: u64,
        error: u64,
        address: usize,
    },
}

/// Errors while constructing a protected user process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessError {
    EmptyProgram,
    ProgramTooLarge,
    InvalidUserRegion,
    UnsupportedCapability,
    NoKernelStack,
}

/// A small, single-address-space user process used by the Black Tea proof image.
///
/// The process receives one code page and one stack page. All kernel mappings remain
/// supervisor-only, and the process owns a separate kernel stack for ring transitions.
pub struct UserProcess<'a> {
    _code: Box<UserPage>,
    _stack: Box<UserPage>,
    address_space: paging::UserAddressSpace,
    kernel_stack: Stack,
    entry: UserEntry,
    allow_i8042: bool,
    _i8042_lease: PhantomData<&'a ()>,
}

impl UserProcess<'static> {
    /// Create a process from a bounded instruction page at `user_base`.
    pub fn new(code: &[u8], user_base: usize) -> Result<Self, ProcessError> {
        Self::new_inner(code, user_base, false)
    }
}

impl<'a> UserProcess<'a> {
    #[cfg(feature = "board-pc")]
    /// Create a process authorized to receive scan codes while `lease` remains live.
    pub fn with_i8042(
        code: &[u8],
        user_base: usize,
        _lease: &'a super::UserI8042Lease,
    ) -> Result<Self, ProcessError> {
        Self::new_inner(code, user_base, true)
    }

    fn new_inner(code: &[u8], user_base: usize, allow_i8042: bool) -> Result<Self, ProcessError> {
        if Board::CAPABILITIES
            .require(Capability::ProtectedAddressSpaces)
            .is_err()
        {
            return Err(ProcessError::UnsupportedCapability);
        }
        if code.is_empty() {
            return Err(ProcessError::EmptyProgram);
        }
        if code.len() > PAGE {
            return Err(ProcessError::ProgramTooLarge);
        }

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

        Ok(Self {
            _code: code_page,
            _stack: stack_page,
            address_space,
            kernel_stack,
            entry,
            allow_i8042,
            _i8042_lease: PhantomData,
        })
    }

    /// Enter ring 3 and return only through the validated exit syscall or a contained fault.
    pub fn run(&mut self) -> ProcessOutcome {
        assert_eq!(
            Board::cpu(),
            0,
            "user demonstration runs only on the boot CPU"
        );
        let mut running = Running {
            resume: Context::default(),
            outcome: None,
            allow_i8042: self.allow_i8042,
        };
        let current = &mut running as *mut Running;
        let flags: u64;
        unsafe { asm!("pushfq", "pop {}", "cli", out(reg) flags, options(nomem)) };
        assert!(
            CURRENT
                .compare_exchange(null_mut(), current, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "a user process is already active",
        );

        let old_root = paging::current_root();
        let cpu = Board::cpu();
        let old_kernel_stack = traps::kernel_stack(cpu);
        let user_context = unsafe {
            Context::new(
                self.kernel_stack
                    .top(),
                enter_user,
                &mut self.entry as *mut UserEntry as *mut (),
            )
        };

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
        unsafe {
            asm!("mov ax, 0x10", "mov ds, ax", "mov es, ax", out("rax") _, options(nomem, nostack, preserves_flags))
        };
        CURRENT.store(null_mut(), Ordering::Release);
        if flags & (1 << 9) != 0 {
            unsafe { asm!("sti", options(nomem, nostack)) };
        }
        running
            .outcome
            .expect("user process returned without exiting or faulting")
    }

    /// The user virtual address where this process's code page begins.
    pub const fn code_address(&self) -> usize {
        self.entry
            .instruction
    }
}

fn zeroed_user_page() -> Box<UserPage> {
    let layout = Layout::new::<UserPage>();
    // SAFETY: the layout has the page's 4 KiB alignment, and every bit pattern of its byte
    // array is valid; the allocation remains owned by the returned Box.
    let page = unsafe { alloc_zeroed(layout) } as *mut UserPage;
    if page.is_null() {
        handle_alloc_error(layout);
    }
    // SAFETY: `alloc_zeroed` returned a live allocation with exactly this layout.
    unsafe { Box::from_raw(page) }
}

extern "C" fn enter_user(arg: *mut ()) -> ! {
    // SAFETY: `UserProcess::run` keeps this entry record alive until the process exits.
    let entry = unsafe { &*(arg as *const UserEntry) };
    unsafe {
        asm!(
            "mov ax, {data:x}",
            "mov ds, ax",
            "mov es, ax",
            data = in(reg) USER_DATA_SELECTOR,
            out("rax") _,
            options(nomem, nostack, preserves_flags),
        );
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
    }
}

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
                Ok(None) => {
                    frame._registers[14] = u64::MAX;
                    return;
                }
                Err(_) => {
                    frame._registers[14] = u64::MAX;
                    return;
                }
            }
        }
    }
    if number == EXIT_SYSCALL && argument <= u32::MAX as u64 {
        finish(ProcessOutcome::Exited(argument as u32));
    }
    frame._registers[14] = u64::MAX;
}

fn current_has_i8042() -> bool {
    let current = CURRENT.load(Ordering::Acquire);
    !current.is_null() && unsafe { (*current).allow_i8042 }
}

pub(super) fn fault(vector: u64, error: u64, address: usize) -> ! {
    finish(ProcessOutcome::Fault {
        vector,
        error,
        address: if vector == 14 { address } else { 0 },
    })
}

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
