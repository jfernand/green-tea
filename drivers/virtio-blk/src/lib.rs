//! virtio-blk: a disk. Its configuration area starts with the capacity, in 512-byte
//! sectors, and it has one queue. Every request is a chain of three buffers:
//!
//! 1. a header the device reads: `{ type, reserved, sector }`;
//! 2. the data: the device writes it for a read, reads it for a write (none for a flush);
//! 3. one status byte the device writes: 0 OK, 1 I/O error, 2 unsupported.
//!
//! This driver does one request at a time and polls for the answer.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use green_tea_io::dma_fence;
use green_tea_virtio::queue::{Buffer, Queue};
use green_tea_virtio::{Transport, driver_ok, negotiate};
use core::fmt;
use core::ptr::{read_volatile, write_volatile};

pub const SECTOR_SIZE: usize = 512;

// Feature bits.
pub const F_RO: u64 = 1 << 5; // read-only disk
pub const F_FLUSH: u64 = 1 << 9; // has a write cache, and the flush command

// Request types.
const T_IN: u32 = 0; // read
const T_OUT: u32 = 1; // write
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8; // a 20-byte serial number

const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;
const S_PENDING: u8 = 0xff; // ours: not a status the device sends

const CONFIG_CAPACITY: usize = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Not a whole number of sectors.
    Unaligned,
    /// Past the end of the disk.
    OutOfRange,
    ReadOnly,
    /// The device said so.
    Io,
    Unsupported,
    /// A status byte the spec doesn't define, or a reply to a request we didn't make.
    Device,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Error::Unaligned => "not a whole number of sectors",
            Error::OutOfRange => "past the end of the disk",
            Error::ReadOnly => "the disk is read-only",
            Error::Io => "I/O error",
            Error::Unsupported => "the device doesn't support that request",
            Error::Device => "the device misbehaved",
        })
    }
}

#[repr(C)]
struct Header {
    kind: u32,
    reserved: u32,
    sector: u64,
}

/// The parts of a request the driver owns: on the heap, so their addresses stay put
/// while the device uses them, wherever the `Blk` itself moves.
#[repr(C)]
struct Request {
    header: Header,
    status: u8,
}

pub struct Blk<T: Transport> {
    transport: T,
    queue: Queue,
    request: *mut Request,
    capacity: u64,
    features: u64,
}

impl<T: Transport> Blk<T> {
    /// Initialize the device behind `transport`, which must be a virtio block device.
    pub fn new(mut transport: T) -> Result<Blk<T>, &'static str> {
        let features = negotiate(&mut transport, F_RO | F_FLUSH)?;
        let queue = Queue::new(&mut transport, 0)?;
        driver_ok(&mut transport);
        let capacity = transport.read_config_u64(CONFIG_CAPACITY);
        let request = Box::into_raw(Box::new(Request {
            header: Header { kind: 0, reserved: 0, sector: 0 },
            status: S_PENDING,
        }));
        Ok(Blk { transport, queue, request, capacity, features })
    }

    /// The disk's size, in sectors.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    pub fn read_only(&self) -> bool {
        self.features & F_RO != 0
    }

    /// Whether writes may sit in a cache until a [`Blk::flush`].
    pub fn write_cache(&self) -> bool {
        self.features & F_FLUSH != 0
    }

    /// Read sectors from `sector` on into `buf`, a whole number of sectors long.
    pub fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), Error> {
        self.check(sector, buf.len())?;
        self.request(T_IN, sector, Some(Buffer::writes(buf)))
    }

    /// Write `buf`, a whole number of sectors long, from `sector` on.
    pub fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), Error> {
        if self.read_only() {
            return Err(Error::ReadOnly);
        }
        self.check(sector, buf.len())?;
        self.request(T_OUT, sector, Some(Buffer::reads(buf)))
    }

    /// Wait until every write so far is on the disk, not just in its cache.
    pub fn flush(&mut self) -> Result<(), Error> {
        if !self.write_cache() {
            return Ok(()); // writes go straight through
        }
        self.request(T_FLUSH, 0, None)
    }

    /// The device's serial number: up to 20 bytes, NUL-padded.
    pub fn id(&mut self) -> Result<[u8; 20], Error> {
        let mut id = [0u8; 20];
        self.request(T_GET_ID, 0, Some(Buffer::writes(&mut id)))?;
        Ok(id)
    }

    fn check(&self, sector: u64, len: usize) -> Result<(), Error> {
        if len == 0 || !len.is_multiple_of(SECTOR_SIZE) {
            return Err(Error::Unaligned);
        }
        let sectors = (len / SECTOR_SIZE) as u64;
        match sector.checked_add(sectors) {
            Some(end) if end <= self.capacity => Ok(()),
            _ => Err(Error::OutOfRange),
        }
    }

    /// Make one request and spin until the device answers.
    fn request(&mut self, kind: u32, sector: u64, data: Option<Buffer>) -> Result<(), Error> {
        let request = self.request;
        unsafe {
            write_volatile(&raw mut (*request).header, Header { kind, reserved: 0, sector });
            write_volatile(&raw mut (*request).status, S_PENDING);
        }
        let header = Buffer { address: unsafe { &raw const (*request).header } as u64, length: size_of::<Header>() as u32, device_writes: false };
        let status = Buffer { address: unsafe { &raw const (*request).status } as u64, length: 1, device_writes: true };
        let head = match data {
            Some(data) => self.queue.submit(&self.transport, &[header, data, status]),
            None => self.queue.submit(&self.transport, &[header, status]),
        }
        .map_err(|_| Error::Device)?;

        let done = loop {
            if let Some((done, _written)) = self.queue.pop_used() {
                break done;
            }
            core::hint::spin_loop();
        };
        dma_fence(); // pop_used saw the index; now the data and status it covers
        if done != head {
            return Err(Error::Device);
        }
        match unsafe { read_volatile(&raw const (*request).status) } {
            S_OK => Ok(()),
            S_IOERR => Err(Error::Io),
            S_UNSUPP => Err(Error::Unsupported),
            _ => Err(Error::Device),
        }
    }
}
