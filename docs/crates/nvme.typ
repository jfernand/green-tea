#import "../lib.typ": *

= green-tea-nvme <crate-nvme>

`green-tea-nvme` is a polled driver for an NVMe disk, the kind in most PCs of the last decade. It resets the controller, finds its first namespace, creates one I/O queue, and offers read, write and flush on whole blocks. It does one command at a time and takes no interrupts.

#spec(
  ("Source", [#repo("drivers/nvme/src/lib.rs"), 721 lines]),
  ("Depends on", [nothing; `no_std`]),
  ("Used by", [`green-tea-bare` (the `pc` board), for the `disk` kernel]),
  ("Tests", [12 host tests, against a fake controller; the `disk` kernel on QEMU's `nvme`]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`Nvme::new(base, memory, delay_ms)`], [resets the controller, identifies it, and creates the I/O queues],
  [`Nvme::disable(base, delay_ms)`], [turns a controller off, so that it stops using its memory],
  [`capacity()`, `block_size()`], [the namespace's size in blocks, and the size of a block],
  [`serial()`], [the controller's serial number, without its padding],
  [`read(block, buf)`, `write(block, buf)`], [whole blocks, from a buffer of any alignment],
  [`flush()`], [asks the controller to put what it has cached on the medium],
  [`Memory`], [the five pages the controller reads and writes: four queues and a page for data],
  [`Error`], [`Unaligned`, `OutOfRange`, `Status`, `Timeout`, `Device`],
  [`PCI_CLASS`, `REGISTERS`], [the class code `0x010802`, and how much of the BAR the driver uses],
)

== Queues in memory

The controller reads commands from a *submission queue* and posts results to a *completion queue*, both in memory. A write to a *doorbell* register says a queue has moved. Queue 0 is the admin queue, for setting up, and we create queue 1 for I/O. Each has 16 entries, or fewer if the controller's `CAP` register allows fewer.

A command is 64 bytes, 16 dwords. The fields we use:

#bytefield(caption: "An NVMe command, as this driver fills it",
  ("opcode, id", "dword 0", 2), ("namespace", "1", 2), ("PRP", "6–7", 2), ("cdw10", "10", 2), ("cdw11", "11", 2), ("cdw12", "12", 2),
)

#srcfile("drivers/nvme/src/lib.rs", lines: ((123, 133),), notes: ((128, [The PRP is the address of the data. One is enough, because a transfer never leaves our data page.]),))[```rust
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
```]

== Bringing the controller up

`new` checks that `CAP` offers the NVM command set and 4 KiB pages. It waits for the controller to finish any change of state, clears `CC.EN`, and waits for `CSTS.RDY` to fall, for as long as `CAP.TO` allows. It then writes the admin queues' size and addresses to `AQA`, `ASQ` and `ACQ`, and sets `CC.EN` with entry sizes of 64 and 16 bytes.

Three *Identify* commands follow: the controller, for its serial number; the list of active namespaces, of which we take the first; and that namespace, for its size and block format. The format table holds one entry per format, and we refuse one with metadata or with blocks over a page:

#srcfile("drivers/nvme/src/lib.rs", lines: ((155, 160),), notes: ((157, [The format index is split: four low bits, and two high ones at bits 6–5.]), (160, [`entry[2]` is the block size as a power of two: 9 for 512 bytes, 12 for 4 KiB.]),))[```rust
fn block_size(identify: &[u8]) -> Option<usize> {
    let flbas = identify[26];
    let format = usize::from(flbas & 0xf | (flbas >> 5 & 3) << 4);
    let entry = identify.get(128 + format * 4..128 + format * 4 + 4)?;
    let metadata = u16::from_le_bytes([entry[0], entry[1]]);
    (metadata == 0 && (9..=12).contains(&entry[2])).then(|| 1 << entry[2])
```]

*Create I/O Completion Queue* and *Create I/O Submission Queue* finish the setup, in that order, because a submission queue names the completion queue it reports to.

== A command

`run` writes the 16 dwords at the tail, moves the tail, rings the doorbell and polls:

#srcfile("drivers/nvme/src/lib.rs", lines: ((329, 353),), size: 6.4pt, notes: ((336, [The identifier comes back in the completion.]), (344, [The controller may start at once, so the fence above comes first.]), (351, [It may still write to our memory later, so we send it nothing more.]),))[```rust
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
```]

A completion is new when its *phase bit* differs from what the slot held before. The controller flips the bit each time it wraps, as xHCI does with its cycle bit (@crate-xhci):

#srcfile("drivers/nvme/src/lib.rs", lines: ((356, 380),), size: 6.4pt, notes: ((360, [Bit 16 of the last dword is the phase.]), (370, [Tells the controller the slot is free.]), (372, [With one command at a time, any other id means the controller is confused.]),))[```rust
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
```]

== Reading and writing

Data goes through the driver's own page, so a caller's buffer can be anywhere, including the demand-paged heap, which a device couldn't fault in. A transfer is split into commands of at most one page:

#srcfile("drivers/nvme/src/lib.rs", lines: ((270, 279),), notes: ((271, [Whole blocks, and inside the namespace, before anything is sent.]), (275, [`transfer` puts the block number in `cdw10` and `cdw11`, and the count, minus one, in `cdw12`.]),))[```rust
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
```]

== Exercised by

The host tests run the driver against a fake controller in memory, which does its work when the driver waits. They cover identification, a format with metadata, the split format index, a read across pages, a write that stays inside its blocks, 100 commands past the queues' wrap, requests the driver refuses, a status from the controller, a timeout, and a completion with the wrong id.

#output(title: "cargo test -p green-tea-nvme")[
```
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

The `disk` kernel on the `pc` board runs the same checks as on virtio (@crate-coop), on QEMU's `nvme` device. That test overwrites a sector, and this board can meet a disk that holds someone's data. So `Pc::find_nvme` takes the serial number it wants, starts each controller on the bus to ask it, and turns off the ones that don't match. No block of any of them is read or written:

#srcfile("kernels/coop/src/main.rs", lines: ((314, 324),), size: 6.4pt, notes: ((314, [`DISK_ID` is the serial `xtask` gives its test image. A drive with any other serial is turned off, and nothing has been read or written.]),))[```rust
let mut disk = match Board::find_nvme(DISK_ID) {
    Ok(Some(disk)) => disk,
    Ok(None) => return println!("FAIL: no NVMe disk is the test disk, so none was touched"),
    Err(e) => return println!("FAIL: {e}"),
};
let sectors = disk.capacity();
let serial = core::str::from_utf8(disk.serial()).unwrap_or("?");
println!("nvme: {sectors} blocks of {} bytes, serial {serial:?}", disk.block_size());
if disk.block_size() != SECTOR_SIZE {
    return println!("FAIL: the test expects {SECTOR_SIZE}-byte blocks");
}
```]

#output(title: "cargo xtask kernel --board pc --kernel disk --target x86_64 --headless")[
```
nvme: 64 blocks of 512 bytes, serial "green-tea-disk"
PASS: read 64 sectors over NVMe, and wrote one back
```
]

We then booted the same image with a 32 KiB drive of random bytes and another serial number, and compared its hash before and after:

#output(title: "docs/record.d/nvme.sh: an NVMe drive with another serial")[
```
nvme: passing over "somebodys-ssd"
FAIL: no NVMe disk is the test disk, so none was touched
the disk's 32 KiB are unchanged
```
]

== Limits

- Tested only against QEMU's `nvme` device and a fake controller, never on a physical drive.
- One command at a time, polled, through one page: every transfer is copied, and a large one takes a command per 4 KiB.
- At most eight controllers, and only each one's first active namespace.
- Looking for the test disk resets every NVMe controller up to the one that matches, and leaves the others off. Their data isn't touched, but the kernel can't use them afterwards.
- Blocks of 512 bytes to 4 KiB, with no metadata. Other formats are refused.
- `read` and `write` have no partition table or file system above them: they address the whole namespace.
- A controller in a fatal state (`CSTS.CFS`) is reset, the only way out of that state, and has only the controller's own timeout to recover. If the flag stays set, enabling it reports the error.
- A write protected namespace reports an error status only when a write is attempted.

One queue pair, one page, one guard. @crate-bare is what puts these drivers on a machine.
// chapter-status:begin
// stage: research-done
// revision: 3
// body: 61099342a613
// activity: 2026-11-22 | r2 | stage | green-tea | note | tracking started at early-draft
// activity: 2026-11-23 | r2 | stage | green-tea | note | early-draft -> research-done: not yet reviewed by Javier
// chapter-status:end
