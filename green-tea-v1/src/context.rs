use crate::stack::Stack;
use std::collections::VecDeque;
use std::marker::PhantomPinned;
use std::pin::Pin;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default)]
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
    stack: Stack,
    context: Context,
    func: Option<Box<dyn FnOnce()>>,
    done: bool,
    _pin: PhantomPinned,
}

impl Task {
    pub fn is_done(&self) -> bool {
        self.done
    }
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

/// Create an address-stable task that the scheduler can safely resume.
///
/// ```compile_fail
/// use green_tea_v1::context::spawn;
/// let task = spawn(|| {});
/// let moved = *task;
/// ```
pub fn spawn(func: impl FnOnce() + 'static) -> Pin<Box<Task>> {
    let stack = Stack::new(32 * 1024);
    let mut task = Box::pin(Task {
        stack,
        context: Context::default(),
        func: Some(Box::new(func)),
        done: false,
        _pin: PhantomPinned,
    });
    let task_ptr = unsafe { task.as_mut().get_unchecked_mut() as *mut Task };
    unsafe {
        let sp = initial_sp(&(*task_ptr).stack);
        sp.write(bootstrap_entry as *const () as usize as u64);
        (*task_ptr).context.rsp = sp as u64;
        (*task_ptr).context.r15 = task_ptr as usize as u64;
        (*task_ptr).context.r14 = task_entry as *const () as usize as u64;
    }
    task
}

/// Suspend the current task and switch back to the scheduler.
///
/// # Panics
/// If called outside a task.
pub fn yield_now() {
    assert!(unsafe { !CURRENT.is_null() }, "yield_now called outside a task");
    unsafe { swap_context(&raw mut (*CURRENT).context, &raw const MAIN) };
}

/// Cooperative round-robin scheduler: run the task at the front of the queue until it
/// yields or finishes, then put it at the back, or drop it (and its stack) if it's done.
///
/// A task may call `run`: the outer scheduler's `MAIN` and `CURRENT` are put back on return.
pub fn run(tasks: impl IntoIterator<Item = Pin<Box<Task>>>) {
    let (outer_main, outer_current) = unsafe { ((&raw const MAIN).read(), CURRENT) };
    let mut queue: VecDeque<Pin<Box<Task>>> = tasks
        .into_iter()
        .collect();
    while let Some(mut task) = queue.pop_front() {
        let task_ptr = unsafe { task.as_mut().get_unchecked_mut() as *mut Task };
        unsafe {
            CURRENT = task_ptr;
            swap_context(&raw mut MAIN, &raw const (*task_ptr).context);
            CURRENT = outer_current;
        }
        if !task.as_ref().is_done() {
            queue.push_back(task);
        }
    }
    unsafe { (&raw mut MAIN).write(outer_main) };
}

#[cfg(test)]
mod tests {
    use crate::context::{
        Context, MAIN, Task, bootstrap_entry, initial_sp, run, spawn, swap_context, task_entry,
        yield_now,
    };
    use crate::stack::Stack;
    use std::marker::PhantomPinned;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// `MAIN` and `CURRENT` are process-wide, and the harness runs tests on several threads,
    /// so a test that calls `run` runs alone: in a child process that re-enters this test.
    fn in_child(test: &str, body: impl FnOnce()) {
        const IN_CHILD: &str = "GREEN_TEA_V1_TEST_CHILD";
        if std::env::var_os(IN_CHILD).is_some() {
            body();
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([test, "--exact", "--test-threads=1"])
            .env(IN_CHILD, "1")
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "the child failed: {status}");
    }

    #[test]
    #[should_panic(expected = "yield_now called outside a task")]
    fn yield_outside_a_task_panics() {
        yield_now();
    }

    #[test]
    fn a_nested_run_leaves_the_outer_task_able_to_yield() {
        in_child("context::tests::a_nested_run_leaves_the_outer_task_able_to_yield", || {
            let log = Rc::new(RefCell::new(Vec::new()));
            let outer_log = log.clone();
            let outer = spawn(move || {
                let inner_log = outer_log.clone();
                run([spawn(move || {
                    inner_log.borrow_mut().push("inner 1");
                    yield_now();
                    inner_log.borrow_mut().push("inner 2");
                })]);
                outer_log.borrow_mut().push("outer 1");
                yield_now();
                outer_log.borrow_mut().push("outer 2");
            });
            run([outer]);
            assert_eq!(*log.borrow(), ["inner 1", "inner 2", "outer 1", "outer 2"]);
        });
    }

    #[test_log::test]
    fn test_swap_context() {
        let mut executed = false;
        let executed_ptr = &mut executed as *mut bool;

        let stack = Stack::new(32 * 1024);
        let mut task = Box::pin(Task {
            stack,
            context: Context::default(),
            func: Some(Box::new(move || unsafe {
                *executed_ptr = true;
            })),
            done: false,
            _pin: PhantomPinned,
        });
        let task_ptr = unsafe { task.as_mut().get_unchecked_mut() as *mut Task };

        unsafe {
            let sp = initial_sp(&(*task_ptr).stack);
            sp.write(bootstrap_entry as *const () as usize as u64);
            (*task_ptr).context.rsp = sp as u64;
            (*task_ptr).context.r15 = task_ptr as usize as u64;
            (*task_ptr).context.r14 = task_entry as *const () as usize as u64;

            swap_context(&raw mut MAIN, &raw const (*task_ptr).context);
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
        let mut task = Box::pin(Task {
            stack,
            context: Context::default(),
            func: None,
            done: false,
            _pin: PhantomPinned,
        });
        let task_ptr = unsafe { task.as_mut().get_unchecked_mut() as *mut Task };

        unsafe {
            (*std::ptr::addr_of_mut!(LOG)).clear();
            let sp = initial_sp(&(*task_ptr).stack);
            sp.write(bootstrap_entry as *const () as usize as u64);
            (*task_ptr).context.rsp = sp as u64;
            (*task_ptr).context.r15 = task_ptr as usize as u64;
            (*task_ptr).context.r14 = worker_entry as *const () as usize as u64;
            TASK_CTX = (*task_ptr).context;

            while (*std::ptr::addr_of!(LOG)).last() != Some(&100) {
                swap_context(&raw mut SCHEDULER_CTX, &raw const TASK_CTX);
            }

            assert_eq!(*std::ptr::addr_of!(LOG), vec![1, 2, 3, 100]);
        }
    }
}
