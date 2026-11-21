//! The information a Multiboot2 loader leaves in memory: a total size, then a list of
//! tags, each 8-byte aligned and starting with its type and size. We read the command line,
//! the memory map, the framebuffer and the ACPI root pointer.

#![no_std]

/// What the loader leaves in `eax`.
pub const BOOT_MAGIC: u32 = 0x36d7_6289;

const TAG_END: u32 = 0;
const TAG_CMDLINE: u32 = 1;
const TAG_MEMORY_MAP: u32 = 6;
const TAG_FRAMEBUFFER: u32 = 8;
const TAG_ACPI_OLD: u32 = 14;
const TAG_ACPI_NEW: u32 = 15;

/// A memory map entry's type for RAM that is free to use.
const AVAILABLE: u32 = 1;
/// A framebuffer tag's type for direct RGB.
const FRAMEBUFFER_RGB: u8 = 1;

/// The boot information, as bytes.
#[derive(Clone, Copy)]
pub struct Info<'a>(&'a [u8]);

/// One range of the memory map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub base: u64,
    pub len: u64,
    pub available: bool,
}

/// The display the loader set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Display {
    pub addr: u64,
    /// Bytes from the start of one row to the start of the next.
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
    pub bpp: u8,
    /// Whether a pixel is a little-endian 0x00RRGGBB.
    pub xrgb: bool,
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

impl<'a> Info<'a> {
    /// The information in `bytes`, which start with its total size. `None` if that size
    /// doesn't fit.
    pub fn new(bytes: &'a [u8]) -> Option<Info<'a>> {
        let total = u32_at(bytes, 0)? as usize;
        (total >= 8).then(|| bytes.get(..total).map(Info))?
    }

    /// # Safety
    /// `addr` must be the address a Multiboot2 loader passed, mapped and unchanged for `'a`.
    pub unsafe fn from_addr(addr: usize) -> Option<Info<'a>> {
        // SAFETY: the caller's promise; the first word is the total size.
        unsafe {
            let total = (addr as *const u32).read_unaligned() as usize;
            Info::new(core::slice::from_raw_parts(addr as *const u8, total))
        }
    }

    /// The body of the first tag of type `kind`.
    fn tag(&self, kind: u32) -> Option<&'a [u8]> {
        let mut at = 8;
        loop {
            let (found, size) = (u32_at(self.0, at)?, u32_at(self.0, at + 4)? as usize);
            if found == TAG_END || size < 8 {
                return None;
            }
            if found == kind {
                return self.0.get(at + 8..at + size);
            }
            at = at.checked_add(size)?.checked_add(7)? & !7;
        }
    }

    /// The command line the loader was told to pass.
    pub fn cmdline(&self) -> &'a str {
        let body = self.tag(TAG_CMDLINE).unwrap_or(&[]);
        let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
        core::str::from_utf8(&body[..end]).unwrap_or("")
    }

    /// The value of `key=value` on the command line.
    pub fn arg(&self, key: &str) -> Option<&'a str> {
        self.cmdline().split_ascii_whitespace().find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
    }

    /// Every range of the memory map.
    pub fn regions(&self) -> impl Iterator<Item = Region> + 'a {
        let body = self.tag(TAG_MEMORY_MAP).unwrap_or(&[]);
        let entry = u32_at(body, 0).map_or(0, |n| n as usize);
        let entries = if entry >= 24 { body.get(8..).unwrap_or(&[]).chunks_exact(entry) } else { [].chunks_exact(1) };
        entries.filter_map(|e| {
            Some(Region { base: u64_at(e, 0)?, len: u64_at(e, 8)?, available: u32_at(e, 16)? == AVAILABLE })
        })
    }

    /// The end of the free RAM that `addr` is in.
    pub fn free_end(&self, addr: u64) -> Option<u64> {
        self.regions()
            .find(|r| r.available && addr >= r.base && addr - r.base < r.len)
            .map(|r| r.base + r.len)
    }

    pub fn display(&self) -> Option<Display> {
        let b = self.tag(TAG_FRAMEBUFFER)?;
        let (bpp, kind) = (*b.get(20)?, *b.get(21)?);
        let xrgb = kind == FRAMEBUFFER_RGB && bpp == 32 && matches!(b.get(24..30)?, [16, 8, 8, 8, 0, 8]);
        Some(Display {
            addr: u64_at(b, 0)?,
            pitch: u32_at(b, 8)?,
            width: u32_at(b, 12)?,
            height: u32_at(b, 16)?,
            bpp,
            xrgb,
        })
    }

    /// The copy of the ACPI root pointer (RSDP), the newer form if there is one.
    pub fn rsdp(&self) -> Option<&'a [u8]> {
        self.tag(TAG_ACPI_NEW).or_else(|| self.tag(TAG_ACPI_OLD))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    fn tag(out: &mut Vec<u8>, kind: u32, body: &[u8]) {
        out.extend(kind.to_le_bytes());
        out.extend((8 + body.len() as u32).to_le_bytes());
        out.extend(body);
        while !out.len().is_multiple_of(8) {
            out.push(0);
        }
    }

    fn info(tags: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut out = std::vec![0u8; 8];
        for (kind, body) in tags {
            tag(&mut out, *kind, body);
        }
        tag(&mut out, TAG_END, &[]);
        let total = out.len() as u32;
        out[..4].copy_from_slice(&total.to_le_bytes());
        out
    }

    fn map(entries: &[(u64, u64, u32)]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(24u32.to_le_bytes());
        b.extend(0u32.to_le_bytes());
        for (base, len, kind) in entries {
            b.extend(base.to_le_bytes());
            b.extend(len.to_le_bytes());
            b.extend(kind.to_le_bytes());
            b.extend(0u32.to_le_bytes());
        }
        b
    }

    fn framebuffer(bpp: u8, kind: u8, fields: [u8; 6]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(0x40_0000_0000u64.to_le_bytes());
        b.extend(7680u32.to_le_bytes());
        b.extend(1920u32.to_le_bytes());
        b.extend(1080u32.to_le_bytes());
        b.extend([bpp, kind, 0, 0]);
        b.extend(fields);
        b
    }

    #[test]
    fn finds_an_argument_on_the_command_line() {
        let bytes = info(&[(TAG_CMDLINE, b"quiet test=slab\0".to_vec())]);
        let info = Info::new(&bytes).unwrap();
        assert_eq!(info.cmdline(), "quiet test=slab");
        assert_eq!(info.arg("test"), Some("slab"));
        assert_eq!(info.arg("tes"), None);
        assert_eq!(info.arg("quiet"), None);
    }

    #[test]
    fn walks_past_a_tag_that_needs_padding() {
        let bytes = info(&[(99, std::vec![1, 2, 3]), (TAG_CMDLINE, b"x\0".to_vec())]);
        assert_eq!(Info::new(&bytes).unwrap().cmdline(), "x");
    }

    #[test]
    fn free_end_is_the_end_of_the_free_range_holding_the_address() {
        let body = map(&[(0, 0x9f000, 1), (0x10_0000, 0x100_0000, 1), (0x110_0000, 0x1000, 2)]);
        let bytes = info(&[(TAG_MEMORY_MAP, body)]);
        let info = Info::new(&bytes).unwrap();
        assert_eq!(info.regions().count(), 3);
        assert_eq!(info.free_end(0x10_0000), Some(0x110_0000));
        assert_eq!(info.free_end(0x110_0000), None);
    }

    #[test]
    fn reads_a_display_above_4_gib() {
        let bytes = info(&[(TAG_FRAMEBUFFER, framebuffer(32, 1, [16, 8, 8, 8, 0, 8]))]);
        let display = Info::new(&bytes).unwrap().display().unwrap();
        let want = Display { addr: 0x40_0000_0000, pitch: 7680, width: 1920, height: 1080, bpp: 32, xrgb: true };
        assert_eq!(display, want);
    }

    #[test]
    fn a_display_with_red_and_blue_swapped_is_not_xrgb() {
        let bytes = info(&[(TAG_FRAMEBUFFER, framebuffer(32, 1, [0, 8, 8, 8, 16, 8]))]);
        assert!(!Info::new(&bytes).unwrap().display().unwrap().xrgb);
    }

    #[test]
    fn prefers_the_newer_acpi_pointer() {
        let bytes = info(&[(TAG_ACPI_OLD, b"old".to_vec()), (TAG_ACPI_NEW, b"new".to_vec())]);
        assert_eq!(Info::new(&bytes).unwrap().rsdp(), Some(&b"new"[..]));
    }

    #[test]
    fn a_size_past_the_buffer_is_refused() {
        assert!(Info::new(&[64, 0, 0, 0, 0, 0, 0, 0]).is_none());
    }

    #[test]
    fn a_truncated_tag_ends_the_walk() {
        let mut bytes = std::vec![16u8, 0, 0, 0, 0, 0, 0, 0];
        bytes.extend(TAG_CMDLINE.to_le_bytes());
        bytes.extend(4u32.to_le_bytes()); // shorter than a tag's own header
        assert_eq!(Info::new(&bytes).unwrap().cmdline(), "");
    }
}
