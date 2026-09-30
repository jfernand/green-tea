use libc::{MAP_ANON, MAP_PRIVATE, PROT_NONE, PROT_READ, PROT_WRITE};
use tracing::info;

// The stack grows downwards
pub struct Stack {
    pub(crate) base: *mut u8,
    len: usize,
}

impl Stack {
    pub(crate) fn new(size: usize) -> Stack {
        let guard = page_size(); // 16 KiB on Apple Silicon
        info!("guard: {}k", guard / 1024);
        let len = guard + size;

        // Reserve everything, with no access at all...
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                PROT_NONE,
                MAP_PRIVATE | MAP_ANON,
                -1,
                0,
            )
        } as *mut u8;
        // ...then open up everything above the guard page.
        unsafe {
            libc::mprotect(
                base.add(guard) as *mut libc::c_void,
                size,
                PROT_READ | PROT_WRITE,
            )
        };

        Stack { base, len }
    }

    pub(crate) fn top(&self) -> *const u8 {
        (self.base as usize + self.len) as *const u8 // sp starts here and grows down
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, self.len) };
    }
}

fn page_size() -> usize {
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

#[cfg(test)]
mod tests {
    use crate::stack::Stack;

    #[test]
    fn test_stack_creation() {
        let stack = Stack::new(4096);
        assert!(stack.top() > std::ptr::null());
    }
}
