use libc::{MAP_ANON, MAP_FAILED, MAP_PRIVATE, PROT_NONE, PROT_READ, PROT_WRITE};
use std::io;
use tracing::info;

// The stack grows downwards
pub struct Stack {
    pub(crate) base: *mut u8,
    len: usize,
}

impl Stack {
    /// A stack of `size` usable bytes, with one inaccessible guard page below it.
    ///
    /// # Panics
    /// If the kernel refuses the mapping.
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
        };
        // mmap signals failure with MAP_FAILED, which is (void*)-1, not null.
        if base == MAP_FAILED {
            panic!("mmap of a {len}-byte stack failed: {}", io::Error::last_os_error());
        }
        let base = base as *mut u8;

        // ...then open up everything above the guard page.
        let rc = unsafe {
            libc::mprotect(
                base.add(guard) as *mut libc::c_void,
                size,
                PROT_READ | PROT_WRITE,
            )
        };
        if rc != 0 {
            let err = io::Error::last_os_error(); // before munmap can overwrite errno
            unsafe { libc::munmap(base as *mut libc::c_void, len) };
            panic!("mprotect of a {size}-byte stack failed: {err}");
        }

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

    #[test]
    #[should_panic(expected = "mmap of a")]
    fn a_failed_mapping_panics() {
        // Bigger than any x86_64 user address space.
        Stack::new(1 << 60);
    }
}
