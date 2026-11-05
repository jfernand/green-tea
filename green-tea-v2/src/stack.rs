use libc::{MAP_ANON, MAP_FAILED, MAP_PRIVATE, PROT_NONE, PROT_READ, PROT_WRITE};
use std::io;
use tracing::info;

// The stack grows downwards
pub struct Stack {
    base: *mut u8,
    len: usize,
}

impl Stack {
    /// A stack of at least `size` usable bytes (rounded up to whole pages), with one
    /// inaccessible guard page below it, so an overflow faults instead of scribbling
    /// over whatever is mapped next.
    ///
    /// # Panics
    /// If the kernel refuses the mapping, like `Box::new` aborts when allocation fails.
    pub(crate) fn new(size: usize) -> Stack {
        let guard = page_size(); // 16 KiB on Apple Silicon
        info!("guard: {}k", guard / 1024);
        // mprotect works on whole pages, so give the usable part whole pages too.
        let size = size
            .checked_next_multiple_of(guard)
            .expect("stack size overflows usize");
        let len = guard.checked_add(size).expect("stack size overflows usize");

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

    pub(crate) fn top(&self) -> *mut u8 {
        (self.base as usize + self.len) as *mut u8 // sp starts here and grows down
    }

    /// The addresses of the guard page.
    #[cfg(test)]
    pub(crate) fn guard(&self) -> std::ops::Range<usize> {
        self.base as usize..self.base as usize + page_size()
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
    use crate::stack::{Stack, page_size};

    #[test]
    fn test_stack_creation() {
        let stack = Stack::new(4096);
        assert!(!stack.top().is_null());
    }

    #[test]
    fn size_is_rounded_up_to_whole_pages() {
        let page = page_size();
        let stack = Stack::new(100);
        assert_eq!(stack.len, 2 * page); // guard + one usable page
        assert_eq!(stack.top() as usize % page, 0);

        let stack = Stack::new(page + 1);
        assert_eq!(stack.len, 3 * page);
    }

    #[test]
    #[should_panic(expected = "mmap of a")]
    fn a_failed_mapping_panics() {
        // Bigger than any user address space (47–48 bits on these targets).
        Stack::new(1 << 60);
    }

    #[test]
    #[should_panic(expected = "overflows usize")]
    fn an_absurd_size_panics_instead_of_wrapping() {
        Stack::new(usize::MAX);
    }
}
