use crate::stack::Stack;

#[allow(dead_code)]
#[derive(Debug, Default)]
#[repr(C)]
pub struct Context {
    rsp: u64, // [rdi + 0x00] stack pointer: where execution resumes
    r15: u64, // [rdi + 0x08] callee saved register; Task *
    r14: u64, // etc. callee saved register; entry function pointer
    r13: u64, // saved registers by calling convention.
    r12: u64,
    rbx: u64,
    rbp: u64,
}

#[allow(dead_code)]
unsafe extern "C" {
    pub fn swap_context(from: *mut Context, to: *const Context);
    fn bootstrap_entry() -> !; // never returns, runs ud2 instruction
}

pub struct Task {
    #[allow(dead_code)]
    stack: Stack,
    pub context: Context,
    func: Option<Box<dyn FnOnce()>>,
    pub done: bool,
}

extern "C" fn task_entry(task: *mut Task) -> ! {
    unsafe {
        if let Some(func) = (*task)
            .func
            .take()
        {
            func();
        }
        (*task).done = true;
        let mut dummy = Context::default();
        swap_context(&mut dummy, &raw const MAIN);
        unreachable!()
    }
}

pub static mut MAIN: Context = unsafe { std::mem::zeroed() };
pub static mut CURRENT: *mut Task = core::ptr::null_mut();

/// Where a new task's `rsp` starts. The slot it points to holds the address the first
/// `swap_context` will `ret` into (`bootstrap_entry`).
///
/// That slot must sit at an address ≡ 8 (mod 16): after `ret` pops it, rsp ≡ 0, and
/// `call r14` then pushes to ≡ 8, which is what the SysV ABI expects at function entry.
/// `bootstrap_entry` also does `and rsp, -16` before the call, so this is belt and braces.
fn initial_sp(stack: &Stack) -> *mut u64 {
    let top = (stack.top() as usize) & !0xF;
    (top - 24) as *mut u64
}

pub fn spawn(func: impl FnOnce() + 'static) -> Box<Task> {
    let stack = Stack::new(32 * 1024);
    let mut task = Box::new(Task {
        stack,
        context: Context::default(),
        func: Some(Box::new(func)),
        done: false,
    });
    unsafe {
        let sp = initial_sp(&task.stack);
        sp.write(bootstrap_entry as *const () as usize as u64);
        task.context
            .rsp = sp as u64;
    }
    task.context
        .r15 = &raw mut *task as usize as u64; // the first switch "returns" here
    task.context
        .r14 = task_entry as *const () as usize as u64; // and then jumps here
    task
}

pub fn yield_now() {
    unsafe { swap_context(&raw mut (*CURRENT).context, &raw const MAIN) };
}

#[cfg(test)]
mod tests {
    use crate::context::{
        Context, MAIN, Task, bootstrap_entry, initial_sp, swap_context, task_entry,
    };
    use crate::stack::Stack;

    #[test_log::test]
    fn test_swap_context() {
        let mut executed = false;
        let executed_ptr = &mut executed as *mut bool;

        let stack = Stack::new(32 * 1024);
        let mut task = Box::new(Task {
            stack,
            context: Context::default(),
            func: Some(Box::new(move || unsafe {
                *executed_ptr = true;
            })),
            done: false,
        });

        unsafe {
            let sp = initial_sp(&task.stack);
            sp.write(bootstrap_entry as *const () as usize as u64);
            task.context
                .rsp = sp as u64;
            task.context
                .r15 = &raw mut *task as usize as u64;
            task.context
                .r14 = task_entry as *const () as usize as u64;

            swap_context(&raw mut MAIN, &raw const task.context);
        }

        assert!(executed);
    }

    #[test_log::test]
    fn test_multiple_yields() {
        static mut TASK_CTX: Context = Context {
            rsp: 0,
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            rbx: 0,
            rbp: 0,
        };
        static mut SCHEDULER_CTX: Context = Context {
            rsp: 0,
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            rbx: 0,
            rbp: 0,
        };

        static mut LOG: Vec<i32> = Vec::new();

        extern "C" fn worker_entry(_task: *mut Task) -> ! {
            for i in 1..=3 {
                unsafe {
                    (*std::ptr::addr_of_mut!(LOG)).push(i);
                    swap_context(&raw mut TASK_CTX, &raw const SCHEDULER_CTX);
                }
            }
            unsafe {
                (*std::ptr::addr_of_mut!(LOG)).push(100);
                let mut dummy = Context::default();
                swap_context(&mut dummy, &raw const SCHEDULER_CTX);
                unreachable!()
            }
        }

        let stack = Stack::new(32 * 1024);
        let mut task = Box::new(Task {
            stack,
            context: Context::default(),
            func: None,
            done: false,
        });

        unsafe {
            (*std::ptr::addr_of_mut!(LOG)).clear();
            let sp = initial_sp(&task.stack);
            sp.write(bootstrap_entry as *const () as usize as u64);
            task.context
                .rsp = sp as u64;
            task.context
                .r15 = &raw mut *task as usize as u64;
            task.context
                .r14 = worker_entry as *const () as usize as u64;
            TASK_CTX = task.context;

            while (*std::ptr::addr_of!(LOG)).last() != Some(&100) {
                swap_context(&raw mut SCHEDULER_CTX, &raw const TASK_CTX);
            }

            assert_eq!(*std::ptr::addr_of!(LOG), vec![1, 2, 3, 100]);
        }
    }
}
