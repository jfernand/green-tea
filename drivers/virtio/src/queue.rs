//! A split virtqueue: how a driver and a virtio device pass buffers back and forth. It
//! lives in guest memory, in three parts:
//!
//! - the descriptor table: one entry per buffer `{ address, length, flags, next }`.
//!   Entries chain with `next` into one request: for a disk read, a header the device
//!   reads, the data buffer it writes, and a status byte it writes;
//! - the available ring (the "driver area"): the driver writes the first descriptor of
//!   each new chain there, then bumps the ring's index;
//! - the used ring (the "device area"): the device writes the head of each chain it has
//!   finished, and how many bytes it wrote, then bumps its index.
//!
//! Each side only ever writes its own ring, and indexes are free-running `u16`s, taken
//! modulo the queue size for a slot. A chain is the driver's again once it shows up in
//! the used ring.
//!
//! The driver here polls the used ring, so it asks the device not to interrupt
//! (`AVAIL_F_NO_INTERRUPT`).

use crate::Transport;
use green_tea_io::dma_fence;
use alloc::boxed::Box;
use core::ptr::{addr_of_mut, read_volatile, write_volatile};

/// Descriptors per queue. QEMU's queues allow up to 256 or more; a driver that does one
/// request at a time needs 3.
pub const SIZE: u16 = 16;
const N: usize = SIZE as usize;

const DESC_F_NEXT: u16 = 1; // the chain continues at `next`
const DESC_F_WRITE: u16 = 2; // the device writes this buffer (otherwise it reads it)
const AVAIL_F_NO_INTERRUPT: u16 = 1;

#[repr(C)]
struct Descriptor {
    address: u64,
    length: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct Available {
    flags: u16,
    index: u16,
    ring: [u16; N],
    used_event: u16, // only with the EVENT_IDX feature, which we don't negotiate
}

#[repr(C)]
struct UsedElement {
    id: u32, // the chain's head descriptor (a u32 for alignment)
    length: u32,
}

#[repr(C)]
struct Used {
    flags: u16,
    index: u16,
    ring: [UsedElement; N],
    avail_event: u16,
}

/// All three parts in one allocation. The spec asks for 16-byte alignment for the
/// descriptor table, 2 for the available ring and 4 for the used ring; repr(C) and the
/// field types take care of the last two.
#[repr(C, align(16))]
struct Rings {
    descriptors: [Descriptor; N],
    available: Available,
    used: Used,
}

/// One buffer of a request.
pub struct Buffer {
    pub address: u64,
    pub length: u32,
    /// Whether the device writes it (data in from a disk, a status byte) rather than
    /// reads it.
    pub device_writes: bool,
}

impl Buffer {
    /// A buffer the device reads.
    ///
    /// # Panics
    /// If `data` is 4 GiB or more, which a descriptor's length can't hold.
    pub fn reads<T: ?Sized>(data: &T) -> Buffer {
        Buffer { address: (data as *const T).cast::<u8>() as u64, length: length(size_of_val(data)), device_writes: false }
    }

    /// A buffer the device writes.
    ///
    /// # Panics
    /// If `data` is 4 GiB or more, which a descriptor's length can't hold.
    pub fn writes<T: ?Sized>(data: &mut T) -> Buffer {
        let length = length(size_of_val(data));
        Buffer { address: (data as *mut T).cast::<u8>() as u64, length, device_writes: true }
    }
}

fn length(size: usize) -> u32 {
    u32::try_from(size).expect("a virtio buffer must be under 4 GiB")
}

pub struct Queue {
    index: u16,
    /// Shared with the device, so only touched through raw pointers and volatile
    /// accesses, never through a reference. Never freed: the device may still be
    /// using it.
    rings: *mut Rings,
    /// Unused descriptors, as a list linked through their `next` fields.
    free_head: u16,
    free_count: u16,
    /// The used ring's index as far as we've read it.
    last_used: u16,
}

impl Queue {
    /// Allocate queue `index` and hand it to the device. Call between
    /// [`crate::negotiate`] and [`crate::driver_ok`].
    pub fn new(transport: &mut impl Transport, index: u16) -> Result<Queue, &'static str> {
        if transport.queue_in_use(index) {
            return Err("the virtio queue is already in use");
        }
        let max = transport.max_queue_size(index);
        if max == 0 {
            return Err("the virtio device has no such queue");
        }
        if max < SIZE {
            return Err("the virtio device's queue is too small");
        }
        // SAFETY: all zeros is a valid Rings (it's all integers).
        let rings = Box::into_raw(unsafe { Box::<Rings>::new_zeroed().assume_init() });
        unsafe {
            for i in 0..N {
                (*rings).descriptors[i].next = (i as u16 + 1) % SIZE;
            }
            (*rings).available.flags = AVAIL_F_NO_INTERRUPT;
            transport.setup_queue(
                index,
                SIZE,
                addr_of_mut!((*rings).descriptors) as u64,
                addr_of_mut!((*rings).available) as u64,
                addr_of_mut!((*rings).used) as u64,
            );
        }
        Ok(Queue { index, rings, free_head: 0, free_count: SIZE, last_used: 0 })
    }

    /// Put a chain of `buffers` in the available ring, and tell the device. Returns the
    /// chain's head, which [`Queue::pop_used`] gives back when the device is done. The
    /// buffers must stay put until then: the device reads and writes them directly.
    pub fn submit(&mut self, transport: &impl Transport, buffers: &[Buffer]) -> Result<u16, &'static str> {
        if buffers.is_empty() || buffers.len() > usize::from(self.free_count) {
            return Err("no room in the virtqueue");
        }
        let rings = self.rings;
        let head = self.free_head;
        let mut current = head;
        for (i, buffer) in buffers.iter().enumerate() {
            unsafe {
                let descriptor = addr_of_mut!((*rings).descriptors[usize::from(current)]);
                let next = read_volatile(addr_of_mut!((*descriptor).next));
                let mut flags = if buffer.device_writes { DESC_F_WRITE } else { 0 };
                if i + 1 < buffers.len() {
                    flags |= DESC_F_NEXT;
                }
                write_volatile(addr_of_mut!((*descriptor).address), buffer.address);
                write_volatile(addr_of_mut!((*descriptor).length), buffer.length);
                write_volatile(addr_of_mut!((*descriptor).flags), flags);
                // The last descriptor's `next` keeps pointing into the free list; without
                // DESC_F_NEXT the device ignores it.
                if i + 1 < buffers.len() {
                    current = next;
                } else {
                    self.free_head = next;
                }
            }
        }
        self.free_count -= buffers.len() as u16;

        unsafe {
            let available = addr_of_mut!((*rings).available);
            let index = read_volatile(addr_of_mut!((*available).index));
            write_volatile(addr_of_mut!((*available).ring[usize::from(index % SIZE)]), head);
            dma_fence(); // the descriptors and ring slot before the index that publishes them
            write_volatile(addr_of_mut!((*available).index), index.wrapping_add(1));
        }
        dma_fence(); // the index before the notification
        transport.notify(self.index);
        Ok(head)
    }

    /// The next chain the device has finished: its head, and how many bytes the device
    /// wrote into it. Its descriptors go back on the free list.
    pub fn pop_used(&mut self) -> Option<(u16, u32)> {
        let rings = self.rings;
        unsafe {
            let used = addr_of_mut!((*rings).used);
            if read_volatile(addr_of_mut!((*used).index)) == self.last_used {
                return None;
            }
            dma_fence(); // the index before the element it covers, and the data written
            let element = addr_of_mut!((*used).ring[usize::from(self.last_used % SIZE)]);
            let head = read_volatile(addr_of_mut!((*element).id)) as u16;
            let length = read_volatile(addr_of_mut!((*element).length));
            self.last_used = self.last_used.wrapping_add(1);

            // Put the chain back at the front of the free list.
            let mut last = head;
            let mut count = 1;
            loop {
                let descriptor = addr_of_mut!((*rings).descriptors[usize::from(last)]);
                if read_volatile(addr_of_mut!((*descriptor).flags)) & DESC_F_NEXT == 0 {
                    write_volatile(addr_of_mut!((*descriptor).next), self.free_head);
                    break;
                }
                last = read_volatile(addr_of_mut!((*descriptor).next));
                count += 1;
            }
            self.free_head = head;
            self.free_count += count;
            Some((head, length))
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;

    #[test]
    fn a_buffer_keeps_its_length() {
        let data = [0u8; 512];
        assert_eq!(Buffer::reads(&data).length, 512);
    }

    #[test]
    #[should_panic(expected = "4 GiB")]
    fn a_buffer_of_4_gib_is_refused() {
        let data = vec![0u8; 1 << 32]; // zeroed on demand: never touched, so never resident
        let _ = Buffer::reads(data.as_slice());
    }

    #[test]
    #[should_panic(expected = "4 GiB")]
    fn a_device_written_buffer_of_4_gib_is_refused() {
        let mut data = vec![0u8; 1 << 32];
        let _ = Buffer::writes(data.as_mut_slice());
    }
}
