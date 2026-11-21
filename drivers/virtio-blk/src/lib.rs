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
use green_tea_virtio::{Transport, driver_ok, failed, negotiate};
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
    /// 4 GiB or more: one request's buffer can't be that long.
    TooLong,
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
            Error::TooLong => "a request must be under 4 GiB",
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
        let queue = Queue::new(&mut transport, 0).inspect_err(|_| failed(&mut transport))?;
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
        let written = self.request(T_IN, sector, Some(Buffer::writes(buf)))?;
        // The device counts what it wrote: the data, then the status byte.
        if (written as usize) < buf.len() + 1 {
            return Err(Error::Device);
        }
        Ok(())
    }

    /// Write `buf`, a whole number of sectors long, from `sector` on.
    pub fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), Error> {
        if self.read_only() {
            return Err(Error::ReadOnly);
        }
        self.check(sector, buf.len())?;
        self.request(T_OUT, sector, Some(Buffer::reads(buf))).map(|_| ())
    }

    /// Wait until every write so far is on the disk, not just in its cache.
    pub fn flush(&mut self) -> Result<(), Error> {
        if !self.write_cache() {
            return Ok(()); // writes go straight through
        }
        self.request(T_FLUSH, 0, None).map(|_| ())
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
        if u32::try_from(len).is_err() {
            return Err(Error::TooLong);
        }
        let sectors = (len / SECTOR_SIZE) as u64;
        match sector.checked_add(sectors) {
            Some(end) if end <= self.capacity => Ok(()),
            _ => Err(Error::OutOfRange),
        }
    }

    /// Make one request and spin until the device answers. Returns how many bytes it says
    /// it wrote, the status byte included.
    fn request(&mut self, kind: u32, sector: u64, data: Option<Buffer>) -> Result<u32, Error> {
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

        let (done, written) = loop {
            if let Some(used) = self.queue.pop_used() {
                break used;
            }
            core::hint::spin_loop();
        };
        dma_fence(); // pop_used saw the index; now the data and status it covers
        if done != head {
            return Err(Error::Device);
        }
        match unsafe { read_volatile(&raw const (*request).status) } {
            S_OK => Ok(written),
            S_IOERR => Err(Error::Io),
            S_UNSUPP => Err(Error::Unsupported),
            _ => Err(Error::Device),
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use core::cell::Cell;
    use green_tea_virtio::{F_VERSION_1, STATUS_ACKNOWLEDGE, STATUS_DRIVER, STATUS_FAILED, STATUS_FEATURES_OK};
    use std::vec;

    const DESC_F_NEXT: u16 = 1;
    const DESC_F_WRITE: u16 = 2;

    /// A disk that answers each request as soon as it's notified: it fills what it may
    /// write with 0xab, and reports status OK.
    struct FakeDisk {
        max_queue: u16,
        capacity: u64,
        /// How many bytes fewer than it wrote the device reports in the used ring.
        short_by: u32,
        status: Cell<u8>,
        rings: Cell<(u64, u64, u64)>,
        notified: Cell<usize>,
    }

    impl FakeDisk {
        fn new() -> FakeDisk {
            FakeDisk {
                max_queue: 16,
                capacity: 8,
                short_by: 0,
                status: Cell::new(0),
                rings: Cell::new((0, 0, 0)),
                notified: Cell::new(0),
            }
        }
    }

    impl Transport for &FakeDisk {
        fn device_features(&mut self) -> u64 {
            F_VERSION_1
        }
        fn set_driver_features(&mut self, _features: u64) {}
        fn status(&self) -> u8 {
            self.status.get()
        }
        fn set_status(&mut self, status: u8) {
            self.status.set(status);
        }
        fn max_queue_size(&mut self, _queue: u16) -> u16 {
            self.max_queue
        }
        fn setup_queue(&mut self, _queue: u16, _size: u16, descriptors: u64, driver: u64, device: u64) {
            self.rings.set((descriptors, driver, device));
        }
        fn config_generation(&self) -> u32 {
            0
        }
        fn read_config_u32(&self, offset: usize) -> u32 {
            (self.capacity >> (offset * 8)) as u32
        }

        fn notify(&self, _queue: u16) {
            self.notified.set(self.notified.get() + 1);
            let (descriptors, driver, device) = self.rings.get();
            unsafe {
                let index = read_volatile((driver + 2) as *const u16);
                let slot = u64::from(index.wrapping_sub(1) % 16);
                let head = read_volatile((driver + 4 + 2 * slot) as *const u16);
                let (mut current, mut written) = (head, 0);
                loop {
                    let descriptor = descriptors + 16 * u64::from(current);
                    let address = read_volatile(descriptor as *const u64);
                    let length = read_volatile((descriptor + 8) as *const u32);
                    let flags = read_volatile((descriptor + 12) as *const u16);
                    let next = read_volatile((descriptor + 14) as *const u16);
                    if flags & DESC_F_WRITE != 0 {
                        let fill = if flags & DESC_F_NEXT != 0 { 0xab } else { S_OK };
                        core::ptr::write_bytes(address as *mut u8, fill, length as usize);
                        written += length;
                    }
                    if flags & DESC_F_NEXT == 0 {
                        break;
                    }
                    current = next;
                }
                write_volatile((device + 4 + 8 * slot) as *mut u32, u32::from(head));
                write_volatile((device + 8 + 8 * slot) as *mut u32, written - self.short_by);
                write_volatile((device + 2) as *mut u16, index);
            }
        }
    }

    #[test]
    fn a_read_returns_what_the_device_wrote() {
        let disk = FakeDisk::new();
        let mut blk = Blk::new(&disk).unwrap();
        let mut buf = [0u8; SECTOR_SIZE];
        assert_eq!(blk.read(0, &mut buf), Ok(()));
        assert_eq!(buf, [0xab; SECTOR_SIZE]);
    }

    #[test]
    fn a_queue_failure_marks_the_device_failed() {
        let disk = FakeDisk { max_queue: 0, ..FakeDisk::new() };
        assert!(Blk::new(&disk).is_err());
        let expected = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_FAILED;
        assert_eq!(disk.status.get(), expected);
    }

    #[test]
    fn a_short_read_reported_ok_is_an_error() {
        let disk = FakeDisk { short_by: 1, ..FakeDisk::new() };
        let mut blk = Blk::new(&disk).unwrap();
        let mut buf = [0u8; SECTOR_SIZE];
        assert_eq!(blk.read(0, &mut buf), Err(Error::Device));
    }

    #[test]
    fn a_write_needs_only_the_status_byte_back() {
        let disk = FakeDisk::new();
        let mut blk = Blk::new(&disk).unwrap();
        assert_eq!(blk.write(0, &[1; SECTOR_SIZE]), Ok(()));
    }

    #[test]
    fn a_read_of_4_gib_is_refused_before_the_device_sees_it() {
        let disk = FakeDisk { capacity: 1 << 40, ..FakeDisk::new() };
        let mut blk = Blk::new(&disk).unwrap();
        let mut buf = vec![0u8; 1 << 32]; // zeroed on demand: never touched, so never resident
        assert_eq!(blk.read(0, &mut buf), Err(Error::TooLong));
        assert_eq!(disk.notified.get(), 0, "the device was given a shortened length");
    }
}
