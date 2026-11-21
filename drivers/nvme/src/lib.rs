//! A polled NVMe driver: the disk in most PCs of the last decade. The controller reads
//! commands from a submission queue in memory and writes results to a completion queue; a
//! write to a doorbell register tells it a queue has moved. There is one admin queue, for
//! setting up, and we create one I/O queue.
//!
//! This driver does one command at a time and polls for its completion. Data goes through
//! one page of its own, so a caller's buffer can be anywhere and of any alignment, and a
//! command never needs more than one PRP (the controller's scatter list entry).

#![no_std]

use core::fmt;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, fence};

/// PCI class code of an NVMe controller: mass storage, non-volatile memory, NVMe (in the
/// top three bytes of configuration space's dword at offset 8).
pub const PCI_CLASS: u32 = 0x0001_0802;
/// How much of its BAR the driver uses: the registers and the first doorbells.
pub const REGISTERS: usize = 0x2000;

const PAGE: usize = 4096;
const QUEUE_DEPTH: u16 = 16;
const SQ_ENTRY: usize = 64;
const CQ_ENTRY: usize = 16;
const TIMEOUT_MS: u32 = 10_000;

// Registers.
const CAP: usize = 0x00;
const CC: usize = 0x14;
const CSTS: usize = 0x1c;
const AQA: usize = 0x24;
const ASQ: usize = 0x28;
const ACQ: usize = 0x30;
const DOORBELLS: usize = 0x1000;

const CC_ENABLE: u32 = 1;
/// Submission entries of 2^6 bytes, completion entries of 2^4.
const CC_ENTRY_SIZES: u32 = 6 << 16 | 4 << 20;
const CSTS_READY: u32 = 1 << 0;
const CSTS_FATAL: u32 = 1 << 1;
const CAP_NVM_COMMANDS: u64 = 1 << 37;

// Admin commands.
const CREATE_IO_SQ: u32 = 0x01;
const CREATE_IO_CQ: u32 = 0x05;
const IDENTIFY: u32 = 0x06;
// I/O commands.
const FLUSH: u32 = 0x00;
const WRITE: u32 = 0x01;
const READ: u32 = 0x02;

const IDENTIFY_NAMESPACE: u32 = 0;
const IDENTIFY_CONTROLLER: u32 = 1;
const IDENTIFY_ACTIVE_NAMESPACES: u32 = 2;

const ADMIN: usize = 0;
const IO: usize = 1;

/// The memory the controller reads and writes: four queues and a page for data.
#[repr(C, align(4096))]
pub struct Memory(pub [u8; 5 * PAGE]);

impl Memory {
    pub const fn new() -> Memory {
        Memory([0; 5 * PAGE])
    }
}

impl Default for Memory {
    fn default() -> Memory {
        Memory::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Not a whole number of blocks.
    Unaligned,
    /// Past the end of the namespace.
    OutOfRange,
    /// The controller refused a command, with this status.
    Status(u16),
    /// A command never completed. The controller may still use the driver's memory, so
    /// the driver won't send another.
    Timeout,
    /// A completion for a command we didn't send.
    Device,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Unaligned => f.write_str("not a whole number of blocks"),
            Error::OutOfRange => f.write_str("past the end of the disk"),
            Error::Status(s) => write!(f, "the controller answered with status {s:#x}"),
            Error::Timeout => f.write_str("the controller stopped answering"),
            Error::Device => f.write_str("the controller misbehaved"),
        }
    }
}

fn read32(address: usize) -> u32 {
    // SAFETY: the caller's address is a register of the mapped controller, or our memory.
    unsafe { read_volatile(address as *const u32) }
}

fn write32(address: usize, value: u32) {
    // SAFETY: as for `read32`.
    unsafe { write_volatile(address as *mut u32, value) }
}

fn read64(address: usize) -> u64 {
    u64::from(read32(address)) | u64::from(read32(address + 4)) << 32
}

fn write64(address: usize, value: u64) {
    write32(address, value as u32);
    write32(address + 4, (value >> 32) as u32);
}

/// A command's 16 dwords. The identifier goes in when it is sent.
fn command(opcode: u32, namespace: u32, prp: u64, cdw10: u32, cdw11: u32, cdw12: u32) -> [u32; 16] {
    let mut c = [0; 16];
    c[0] = opcode;
    c[1] = namespace;
    c[6] = prp as u32;
    c[7] = (prp >> 32) as u32;
    c[10] = cdw10;
    c[11] = cdw11;
    c[12] = cdw12;
    c
}

/// A submission queue and the completion queue it reports to.
struct Queue {
    sq: usize,
    cq: usize,
    depth: u16,
    tail: u16,
    head: u16,
    /// The phase bit a new completion has. It flips each time the queue wraps.
    phase: u32,
    next_id: u16,
}

impl Queue {
    fn new(sq: usize, cq: usize, depth: u16) -> Queue {
        Queue { sq, cq, depth, tail: 0, head: 0, phase: 1, next_id: 0 }
    }
}

/// What the namespace's identify data says about its blocks: their size, or `None` for a
/// format we can't use (one with metadata, or blocks over a page).
fn block_size(identify: &[u8]) -> Option<usize> {
    let flbas = identify[26];
    let format = usize::from(flbas & 0xf | (flbas >> 5 & 3) << 4);
    let entry = identify.get(128 + format * 4..128 + format * 4 + 4)?;
    let metadata = u16::from_le_bytes([entry[0], entry[1]]);
    (metadata == 0 && (9..=12).contains(&entry[2])).then(|| 1 << entry[2])
}

/// A running controller, with its first namespace.
pub struct Nvme {
    base: usize,
    doorbell_stride: usize,
    queues: [Queue; 2],
    data: usize,
    delay_ms: fn(u64),
    dead: bool,
    namespace: u32,
    blocks: u64,
    block_size: usize,
    serial: [u8; 20],
}

impl Nvme {
    /// Reset the controller, set up its queues and find its first namespace.
    ///
    /// # Safety
    /// `base` must be the controller's mapped registers, with its PCI function enabled for
    /// memory and DMA, and nothing else may be using it. `memory` must be identity-mapped
    /// and used by nothing else, for good.
    pub unsafe fn new(base: usize, memory: *mut Memory, delay_ms: fn(u64)) -> Result<Nvme, &'static str> {
        let cap = read64(base + CAP);
        if cap & CAP_NVM_COMMANDS == 0 {
            return Err("the NVMe controller has no NVM command set");
        }
        if cap >> 48 & 0xf != 0 {
            return Err("the NVMe controller doesn't support 4 KiB pages");
        }
        let depth = QUEUE_DEPTH.min((cap & 0xffff) as u16 + 1);
        if depth < 2 {
            return Err("the NVMe controller's queues are too short");
        }
        // CAP.TO: how long the controller may take to change state, in units of 500 ms.
        let ready_ms = (cap >> 24 & 0xff).max(1) as u32 * 500;

        // Clearing enable while the controller is on its way up is undefined: let it finish.
        let enabled = read32(base + CC) & CC_ENABLE != 0;
        wait_ready(base, enabled, ready_ms, delay_ms)?;
        write32(base + CC, read32(base + CC) & !CC_ENABLE);
        wait_ready(base, false, ready_ms, delay_ms)?;

        let start = memory as usize;
        // SAFETY: the caller gave us this memory, and the controller is off.
        unsafe { core::ptr::write_bytes(memory, 0, 1) };
        let (admin_sq, admin_cq, io_sq, io_cq) = (start, start + PAGE, start + 2 * PAGE, start + 3 * PAGE);
        let size = u32::from(depth - 1);
        write32(base + AQA, size << 16 | size);
        write64(base + ASQ, admin_sq as u64);
        write64(base + ACQ, admin_cq as u64);
        fence(Ordering::SeqCst);
        write32(base + CC, CC_ENTRY_SIZES | CC_ENABLE);
        wait_ready(base, true, ready_ms, delay_ms)?;

        let mut nvme = Nvme {
            base,
            doorbell_stride: 4 << (cap >> 32 & 0xf),
            queues: [Queue::new(admin_sq, admin_cq, depth), Queue::new(io_sq, io_cq, depth)],
            data: start + 4 * PAGE,
            delay_ms,
            dead: false,
            namespace: 0,
            blocks: 0,
            block_size: 0,
            serial: [0; 20],
        };
        const FAILED: &str = "an NVMe admin command failed";
        nvme.identify(0, IDENTIFY_CONTROLLER).map_err(|_| FAILED)?;
        nvme.serial = nvme.page()[4..24].try_into().map_err(|_| FAILED)?;
        nvme.identify(0, IDENTIFY_ACTIVE_NAMESPACES).map_err(|_| FAILED)?;
        let first = &nvme.page()[..4];
        nvme.namespace = u32::from_le_bytes([first[0], first[1], first[2], first[3]]);
        if nvme.namespace == 0 {
            return Err("the NVMe controller has no namespace");
        }
        nvme.identify(nvme.namespace, IDENTIFY_NAMESPACE).map_err(|_| FAILED)?;
        let blocks = u64::from_le_bytes(nvme.page()[..8].try_into().map_err(|_| FAILED)?);
        let size = block_size(nvme.page()).ok_or("the NVMe namespace has a block format we can't use")?;
        (nvme.blocks, nvme.block_size) = (blocks, size);

        const PHYSICALLY_CONTIGUOUS: u32 = 1;
        let queue = u32::from(depth - 1) << 16 | IO as u32;
        let create_cq = command(CREATE_IO_CQ, 0, io_cq as u64, queue, PHYSICALLY_CONTIGUOUS, 0);
        nvme.run(ADMIN, create_cq).map_err(|_| FAILED)?;
        let create_sq = command(CREATE_IO_SQ, 0, io_sq as u64, queue, (IO as u32) << 16 | PHYSICALLY_CONTIGUOUS, 0);
        nvme.run(ADMIN, create_sq).map_err(|_| FAILED)?;
        Ok(nvme)
    }

    /// The number of blocks in the namespace.
    pub fn capacity(&self) -> u64 {
        self.blocks
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// The controller's serial number, without the spaces that pad it.
    pub fn serial(&self) -> &[u8] {
        let end = self.serial.iter().rposition(|&b| b != b' ' && b != 0).map_or(0, |i| i + 1);
        &self.serial[..end]
    }

    /// Read whole blocks starting at `block`.
    pub fn read(&mut self, block: u64, buf: &mut [u8]) -> Result<(), Error> {
        self.check(block, buf.len())?;
        let per_command = PAGE / self.block_size * self.block_size;
        for (i, chunk) in buf.chunks_mut(per_command).enumerate() {
            let first = block + (i * per_command / self.block_size) as u64;
            self.transfer(READ, first, chunk.len())?;
            chunk.copy_from_slice(&self.page()[..chunk.len()]);
        }
        Ok(())
    }

    /// Write whole blocks starting at `block`.
    pub fn write(&mut self, block: u64, buf: &[u8]) -> Result<(), Error> {
        self.check(block, buf.len())?;
        let per_command = PAGE / self.block_size * self.block_size;
        for (i, chunk) in buf.chunks(per_command).enumerate() {
            let first = block + (i * per_command / self.block_size) as u64;
            // SAFETY: our data page, which the controller isn't using between commands.
            unsafe { core::ptr::copy_nonoverlapping(chunk.as_ptr(), self.data as *mut u8, chunk.len()) };
            self.transfer(WRITE, first, chunk.len())?;
        }
        Ok(())
    }

    /// Ask the controller to put what it has cached on the medium.
    pub fn flush(&mut self) -> Result<(), Error> {
        self.run(IO, command(FLUSH, self.namespace, 0, 0, 0, 0))
    }

    fn check(&self, block: u64, len: usize) -> Result<(), Error> {
        if !len.is_multiple_of(self.block_size) {
            return Err(Error::Unaligned);
        }
        match block.checked_add((len / self.block_size) as u64) {
            Some(end) if end <= self.blocks => Ok(()),
            _ => Err(Error::OutOfRange),
        }
    }

    fn transfer(&mut self, opcode: u32, block: u64, len: usize) -> Result<(), Error> {
        let count = (len / self.block_size) as u32 - 1; // the command counts from zero
        let c = command(opcode, self.namespace, self.data as u64, block as u32, (block >> 32) as u32, count);
        self.run(IO, c)
    }

    fn identify(&mut self, namespace: u32, what: u32) -> Result<(), Error> {
        self.run(ADMIN, command(IDENTIFY, namespace, self.data as u64, what, 0, 0))
    }

    fn page(&self) -> &[u8] {
        // SAFETY: our data page, and no command is running.
        unsafe { core::slice::from_raw_parts(self.data as *const u8, PAGE) }
    }

    fn doorbell(&self, queue: usize, completion: bool) -> usize {
        self.base + DOORBELLS + (2 * queue + usize::from(completion)) * self.doorbell_stride
    }

    /// Send a command and wait for it.
    fn run(&mut self, queue: usize, mut c: [u32; 16]) -> Result<(), Error> {
        if self.dead {
            return Err(Error::Timeout);
        }
        let q = &mut self.queues[queue];
        let id = q.next_id;
        q.next_id = q.next_id.wrapping_add(1);
        c[0] |= u32::from(id) << 16;
        let entry = q.sq + usize::from(q.tail) * SQ_ENTRY;
        for (i, dword) in c.iter().enumerate() {
            write32(entry + i * 4, *dword);
        }
        q.tail = (q.tail + 1) % q.depth;
        let tail = q.tail;
        fence(Ordering::SeqCst);
        write32(self.doorbell(queue, false), u32::from(tail));
        for _ in 0..TIMEOUT_MS {
            if let Some(result) = self.completion(queue, id) {
                return result;
            }
            (self.delay_ms)(1);
        }
        self.dead = true;
        Err(Error::Timeout)
    }

    /// The result of command `id`, if the controller has posted it.
    fn completion(&mut self, queue: usize, id: u16) -> Option<Result<(), Error>> {
        let q = &mut self.queues[queue];
        let entry = q.cq + usize::from(q.head) * CQ_ENTRY;
        let last = read32(entry + 12);
        if last >> 16 & 1 != q.phase {
            return None;
        }
        fence(Ordering::Acquire);
        q.head += 1;
        if q.head == q.depth {
            q.head = 0;
            q.phase ^= 1;
        }
        let head = q.head;
        write32(self.doorbell(queue, true), u32::from(head));
        let status = (last >> 17) as u16;
        Some(if last as u16 != id {
            self.dead = true; // we can't tell what it is doing with our memory
            Err(Error::Device)
        } else if status != 0 {
            Err(Error::Status(status))
        } else {
            Ok(())
        })
    }
}

/// Wait for CSTS.RDY to equal `ready`.
fn wait_ready(base: usize, ready: bool, timeout_ms: u32, delay_ms: fn(u64)) -> Result<(), &'static str> {
    for _ in 0..=timeout_ms {
        let status = read32(base + CSTS);
        if status & CSTS_FATAL != 0 && ready {
            return Err("the NVMe controller reported a fatal error");
        }
        if (status & CSTS_READY != 0) == ready {
            return Ok(());
        }
        delay_ms(1);
    }
    Err("the NVMe controller didn't change state")
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::boxed::Box;
    use std::cell::RefCell;
    use std::vec;
    use std::vec::Vec;

    const BLOCK: usize = 512;
    const BLOCKS: usize = 64;

    /// A controller in memory, which does its work when the driver waits.
    struct Fake {
        registers: Box<[u32; 0x1000]>,
        disk: Vec<u8>,
        queues: [(usize, usize, u32, u32); 2], // sq, cq, sq head, cq tail
        phases: [u32; 2],
        depth: u32,
        /// Answer I/O commands with this status.
        status: u16,
        /// Never complete I/O commands.
        silent: bool,
        /// Answer I/O commands with the wrong identifier.
        wrong_id: bool,
        log: Vec<(u32, u64, u32)>,
        format: u32,
    }

    std::thread_local! {
        static FAKE: RefCell<Option<Fake>> = const { RefCell::new(None) };
    }

    fn base() -> usize {
        FAKE.with(|f| f.borrow().as_ref().unwrap().registers.as_ptr() as usize)
    }

    fn with<R>(body: impl FnOnce(&mut Fake) -> R) -> R {
        FAKE.with(|f| body(f.borrow_mut().as_mut().unwrap()))
    }

    fn step(_ms: u64) {
        with(|f| {
            let registers = f.registers.as_ptr() as usize;
            let at = move |offset: usize| registers + offset;
            let enabled = read32(at(CC)) & CC_ENABLE;
            if enabled != read32(at(CSTS)) & CSTS_READY {
                write32(at(CSTS), enabled);
                if enabled == 1 {
                    f.queues[0] = (read64(at(ASQ)) as usize, read64(at(ACQ)) as usize, 0, 0);
                    f.depth = (read32(at(AQA)) & 0xfff) + 1;
                }
            }
            if enabled == 0 {
                return;
            }
            for q in 0..2 {
                let doorbell = read32(at(DOORBELLS + 8 * q));
                while f.queues[q].2 != doorbell && f.queues[q].0 != 0 {
                    let entry = f.queues[q].0 + f.queues[q].2 as usize * SQ_ENTRY;
                    let c: Vec<u32> = (0..16).map(|i| read32(entry + i * 4)).collect();
                    f.queues[q].2 = (f.queues[q].2 + 1) % f.depth;
                    let status = if q == ADMIN { admin(f, &c) } else { io(f, &c) };
                    if q == IO && f.silent {
                        continue;
                    }
                    let id = (c[0] >> 16) ^ u32::from(q == IO && f.wrong_id);
                    let cq = f.queues[q].1 + f.queues[q].3 as usize * CQ_ENTRY;
                    write32(cq + 12, id | (f.phases[q] | u32::from(status) << 1) << 16);
                    f.queues[q].3 += 1;
                    if f.queues[q].3 == f.depth {
                        f.queues[q].3 = 0;
                        f.phases[q] ^= 1;
                    }
                }
            }
        });
    }

    fn admin(f: &mut Fake, c: &[u32]) -> u16 {
        let prp = (u64::from(c[7]) << 32 | u64::from(c[6])) as usize;
        match c[0] & 0xff {
            IDENTIFY => {
                // SAFETY: the driver's data page.
                let page = unsafe { core::slice::from_raw_parts_mut(prp as *mut u8, PAGE) };
                page.fill(0);
                match c[10] {
                    IDENTIFY_CONTROLLER => {
                        page[4..24].copy_from_slice(b"green-tea-disk      ");
                    }
                    IDENTIFY_ACTIVE_NAMESPACES => page[0] = 1,
                    _ => {
                        page[..8].copy_from_slice(&(BLOCKS as u64).to_le_bytes());
                        page[128..132].copy_from_slice(&f.format.to_le_bytes());
                    }
                }
            }
            CREATE_IO_CQ => f.queues[IO].1 = prp,
            CREATE_IO_SQ => f.queues[IO].0 = prp,
            _ => return 1,
        }
        0
    }

    fn io(f: &mut Fake, c: &[u32]) -> u16 {
        let opcode = c[0] & 0xff;
        let block = u64::from(c[11]) << 32 | u64::from(c[10]);
        f.log.push((opcode, block, c[12]));
        if f.status != 0 {
            return f.status;
        }
        let (start, len) = (block as usize * BLOCK, (c[12] as usize + 1) * BLOCK);
        let prp = (u64::from(c[7]) << 32 | u64::from(c[6])) as usize;
        // SAFETY: the driver's data page, which a command never overruns (asserted).
        unsafe {
            match opcode {
                READ => {
                    assert!(len <= PAGE);
                    core::ptr::copy_nonoverlapping(f.disk[start..start + len].as_ptr(), prp as *mut u8, len);
                }
                WRITE => {
                    assert!(len <= PAGE);
                    core::ptr::copy_nonoverlapping(prp as *const u8, f.disk[start..start + len].as_mut_ptr(), len);
                }
                _ => {}
            }
        }
        0
    }

    fn start(format: u32) -> (Result<Nvme, &'static str>, Box<Memory>) {
        let mut registers = Box::new([0u32; 0x1000]);
        // Queues of 64, 4 KiB pages, NVM commands, a doorbell stride of 4.
        registers[0] = 63 | 1 << 24;
        registers[1] = 1 << 5;
        let disk = (0..BLOCKS * BLOCK).map(|i| (i / BLOCK) as u8 ^ i as u8).collect();
        let fake = Fake {
            registers,
            disk,
            queues: [(0, 0, 0, 0); 2],
            phases: [1; 2],
            depth: 0,
            status: 0,
            silent: false,
            wrong_id: false,
            log: Vec::new(),
            format,
        };
        FAKE.with(|f| *f.borrow_mut() = Some(fake));
        let mut memory = Box::new(Memory::new());
        // SAFETY: the fake's registers, and memory that outlives the driver in each test.
        let nvme = unsafe { Nvme::new(base(), &raw mut *memory, step) };
        (nvme, memory)
    }

    const LBA_512: u32 = 9 << 16;

    #[test]
    fn identifies_the_disk() {
        let (nvme, _memory) = start(LBA_512);
        let nvme = nvme.unwrap();
        assert_eq!(nvme.capacity(), BLOCKS as u64);
        assert_eq!(nvme.block_size(), BLOCK);
        assert_eq!(nvme.serial(), b"green-tea-disk");
    }

    #[test]
    fn a_format_with_metadata_is_refused() {
        let (nvme, _memory) = start(LBA_512 | 8);
        assert!(nvme.is_err());
    }

    #[test]
    fn the_block_format_index_has_high_bits() {
        let mut identify = vec![0u8; PAGE];
        identify[26] = 1 | 1 << 5; // format 17
        identify[128 + 17 * 4 + 2] = 12;
        assert_eq!(block_size(&identify), Some(4096));
    }

    #[test]
    fn a_read_over_several_pages_is_split_and_correct() {
        let (nvme, _memory) = start(LBA_512);
        let mut nvme = nvme.unwrap();
        let mut buf = vec![0u8; 20 * BLOCK];
        nvme.read(3, &mut buf).unwrap();
        assert_eq!(buf, with(|f| f.disk[3 * BLOCK..23 * BLOCK].to_vec()));
        let log = with(|f| f.log.clone());
        assert_eq!(log, [(READ, 3, 7), (READ, 11, 7), (READ, 19, 3)]);
    }

    #[test]
    fn a_write_reaches_only_its_blocks() {
        let (nvme, _memory) = start(LBA_512);
        let mut nvme = nvme.unwrap();
        let before = with(|f| f.disk.clone());
        nvme.write(5, &[0xaa; 2 * BLOCK]).unwrap();
        nvme.flush().unwrap();
        let after = with(|f| f.disk.clone());
        assert!(after[5 * BLOCK..7 * BLOCK].iter().all(|&b| b == 0xaa));
        assert_eq!(after[..5 * BLOCK], before[..5 * BLOCK]);
        assert_eq!(after[7 * BLOCK..], before[7 * BLOCK..]);
        assert_eq!(with(|f| f.log.last().unwrap().0), FLUSH);
    }

    #[test]
    fn the_queues_keep_working_past_a_wrap() {
        let (nvme, _memory) = start(LBA_512);
        let mut nvme = nvme.unwrap();
        let mut buf = [0u8; BLOCK];
        for i in 0..100 {
            nvme.read(i % BLOCKS as u64, &mut buf).unwrap();
            assert_eq!(buf[..], with(|f| f.disk[(i as usize % BLOCKS) * BLOCK..][..BLOCK].to_vec()));
        }
    }

    #[test]
    fn refuses_what_the_device_would() {
        let (nvme, _memory) = start(LBA_512);
        let mut nvme = nvme.unwrap();
        let mut buf = [0u8; 2 * BLOCK];
        assert_eq!(nvme.read(BLOCKS as u64 - 1, &mut buf), Err(Error::OutOfRange));
        assert_eq!(nvme.read(u64::MAX, &mut buf), Err(Error::OutOfRange));
        assert_eq!(nvme.read(0, &mut buf[..100]), Err(Error::Unaligned));
        assert!(with(|f| f.log.is_empty()), "nothing was sent");
    }

    #[test]
    fn a_status_is_reported() {
        let (nvme, _memory) = start(LBA_512);
        let mut nvme = nvme.unwrap();
        with(|f| f.status = 0x281);
        let mut buf = [0u8; BLOCK];
        assert_eq!(nvme.read(0, &mut buf), Err(Error::Status(0x281)));
        with(|f| f.status = 0);
        assert_eq!(nvme.read(0, &mut buf), Ok(()), "a refused command doesn't end the driver");
    }

    #[test]
    fn after_a_timeout_nothing_more_is_sent() {
        let (nvme, _memory) = start(LBA_512);
        let mut nvme = nvme.unwrap();
        with(|f| f.silent = true);
        let mut buf = [0u8; BLOCK];
        assert_eq!(nvme.read(0, &mut buf), Err(Error::Timeout));
        let sent = with(|f| f.log.len());
        assert_eq!(nvme.read(0, &mut buf), Err(Error::Timeout));
        assert_eq!(with(|f| f.log.len()), sent);
    }

    #[test]
    fn a_completion_for_another_command_is_an_error() {
        let (nvme, _memory) = start(LBA_512);
        let mut nvme = nvme.unwrap();
        with(|f| f.wrong_id = true);
        let mut buf = [0u8; BLOCK];
        assert_eq!(nvme.read(0, &mut buf), Err(Error::Device));
    }
}
