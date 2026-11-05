//! QEMU's firmware configuration device (fw_cfg), memory-mapped on the `virt` machine.
//!
//! fw_cfg is a list of numbered items: some fixed (signature, features, file directory) and
//! some named "files", like `etc/ramfb`. You select an item, then read its bytes from the
//! data register, or write it with a DMA request. See qemu's docs/specs/fw_cfg.rst.
//!
//! How the registers are reached differs per machine (memory-mapped on riscv64 and
//! aarch64, I/O ports on x86_64): each platform module provides `FW_CFG` with `select`,
//! `read_u8` and `start_dma`. The protocol on top is the same everywhere.

use crate::platform::FW_CFG;
use core::ptr::read_volatile;
use core::sync::atomic::{Ordering, fence};

const KEY_SIGNATURE: u16 = 0x0000;
const KEY_ID: u16 = 0x0001;
const KEY_FILE_DIR: u16 = 0x0019;

const ID_DMA: u32 = 1 << 1;

const DMA_ERROR: u32 = 1 << 0;
const DMA_SELECT: u32 = 1 << 3;
const DMA_WRITE: u32 = 1 << 4;

fn select(key: u16) {
    FW_CFG.select(key);
}

fn read(buf: &mut [u8]) {
    for byte in buf {
        *byte = FW_CFG.read_u8();
    }
}

fn read_be_u32() -> u32 {
    let mut buf = [0; 4];
    read(&mut buf);
    u32::from_be_bytes(buf)
}

/// Whether fw_cfg is there and supports DMA, which writing a file needs.
pub fn probe() -> Result<(), &'static str> {
    let mut signature = [0; 4];
    select(KEY_SIGNATURE);
    read(&mut signature);
    if &signature != b"QEMU" {
        return Err("no fw_cfg device (bad signature)");
    }
    let mut id = [0; 4];
    select(KEY_ID);
    read(&mut id);
    if u32::from_le_bytes(id) & ID_DMA == 0 {
        // The feature bitmap is one of the few little-endian items.
        return Err("fw_cfg has no DMA interface");
    }
    Ok(())
}

/// A named item in the file directory.
pub struct File {
    pub name: [u8; 56],
    pub size: u32,
    pub select: u16,
}

impl File {
    pub fn name(&self) -> &str {
        let len = self.name.iter().position(|&b| b == 0).unwrap_or(self.name.len());
        core::str::from_utf8(&self.name[..len]).unwrap_or("?")
    }
}

/// Call `f` with each entry of the file directory: a big-endian count, then 64-byte
/// entries `{ size: u32, select: u16, reserved: u16, name: [u8; 56] }`, all big-endian.
pub fn for_each_file(mut f: impl FnMut(&File)) {
    select(KEY_FILE_DIR);
    let count = read_be_u32();
    for _ in 0..count {
        let size = read_be_u32();
        let mut rest = [0; 4];
        read(&mut rest);
        let mut file = File {
            name: [0; 56],
            size,
            select: u16::from_be_bytes([rest[0], rest[1]]),
        };
        read(&mut file.name);
        f(&file);
    }
}

pub fn find_file(name: &str) -> Option<(u16, u32)> {
    let mut found = None;
    for_each_file(|file| {
        if found.is_none() && file.name() == name {
            found = Some((file.select, file.size));
        }
    });
    found
}

/// A DMA request, read by QEMU from guest memory. All fields are big-endian.
#[repr(C, align(16))]
struct DmaAccess {
    control: u32,
    length: u32,
    address: u64,
}

/// Write `data` to item `key` with a DMA request, and wait for it to finish.
pub fn dma_write(key: u16, data: &[u8]) -> Result<(), &'static str> {
    let mut access = DmaAccess {
        control: ((u32::from(key) << 16) | DMA_SELECT | DMA_WRITE).to_be(),
        length: (data.len() as u32).to_be(),
        address: (data.as_ptr() as u64).to_be(),
    };
    let address = &raw mut access as u64;
    fence(Ordering::SeqCst); // the request and the data are in memory before QEMU looks
    FW_CFG.start_dma(address);
    // QEMU clears the control field when it's done, leaving only the error bit if it failed.
    loop {
        let control = u32::from_be(unsafe { read_volatile(&raw const access.control) });
        if control & DMA_ERROR != 0 {
            return Err("fw_cfg DMA write failed");
        }
        if control == 0 {
            return Ok(());
        }
    }
}

/// The memory-mapped register layout used by riscv64 and aarch64 `virt`. The registers are
/// big-endian: the bytes in memory order are the big-endian representation, so values are
/// converted with `to_be` before a volatile store.
#[cfg_attr(target_arch = "x86_64", allow(dead_code))]
pub mod mmio {
    use core::ptr::{read_volatile, write_volatile};

    /// fw_cfg at this base address: data at +0x00, selector at +0x08, DMA address at +0x10.
    pub struct Mmio(pub usize);

    impl Mmio {
        pub fn select(&self, key: u16) {
            unsafe { write_volatile((self.0 + 0x08) as *mut u16, key.to_be()) };
        }

        /// The selected item's next byte.
        pub fn read_u8(&self) -> u8 {
            unsafe { read_volatile(self.0 as *const u8) }
        }

        /// High half first: writing the low half starts the transfer.
        pub fn start_dma(&self, address: u64) {
            unsafe {
                write_volatile((self.0 + 0x10) as *mut u32, ((address >> 32) as u32).to_be());
                write_volatile((self.0 + 0x14) as *mut u32, (address as u32).to_be());
            }
        }
    }
}
