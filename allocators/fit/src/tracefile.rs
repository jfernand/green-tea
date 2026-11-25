//! Trace files: a trace written down, so that it can leave the machine that recorded it.
//!
//! A file is a list of [`Record`]s. There are three kinds: a block was allocated, a block
//! was freed, a block was reallocated. A file has one of two forms, which hold the same
//! records.
//!
//! # The text form
//!
//! One record to a line, fields separated by spaces, addresses in hexadecimal without a
//! prefix, sizes and alignments in decimal:
//!
//! ```text
//! gtt 1                       the first line: the format, and its version
//! # recorded by ...           a line that starts with # is a comment
//! a 80212340 64 8             allocated: address, size, alignment
//! f 80212340                  freed: address
//! r 80212400 80212480 128 16  reallocated: old address, new address, new size, alignment
//! end 3 c2248561940bfcf8      the last line: how many records, and their checksum
//! ```
//!
//! A line may end in `\n` or `\r\n`, and empty lines are skipped. Nothing but comments and
//! empty lines may follow `end`. A file without an `end` line was cut short, and
//! [`read_text`] refuses it, as it refuses one whose count or checksum is wrong.
//!
//! # The binary form
//!
//! The four bytes `GTT\x01`, then the records. Each starts with a byte whose two low bits
//! say what it is (0 freed, 1 allocated, 2 reallocated, 3 the end) and whose six high bits
//! are the base-2 logarithm of the alignment. Addresses are written as their difference
//! from the address written before, since a program's blocks are near one another:
//!
//! ```text
//! allocated    byte, zigzag(address - last), varint(size)
//! freed        byte, zigzag(address - last)
//! reallocated  byte, zigzag(old - last), zigzag(new - old), varint(size)
//! the end      byte, varint(count), the checksum as 8 bytes, least significant first
//! ```
//!
//! A varint is LEB128: seven bits to a byte, least significant first, the high bit set on
//! every byte but the last. Zigzag maps a signed difference to an unsigned number (0, -1,
//! 1, -2 become 0, 1, 2, 3) and writes that as a varint. `last` starts at 0 and is the
//! address a record names, or the new address of a reallocation.
//!
//! # The checksum
//!
//! FNV-1a, 64 bits, over every record in order: the byte `a`, `f` or `r`, then each field
//! of the record as 8 bytes, least significant first. Both forms carry the same checksum.

use crate::trace::Event;
use alloc::vec::Vec;
use core::fmt;

/// One line of a trace file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Record {
    Allocate {
        addr: u64,
        size: u64,
        align: u64,
    },
    Free {
        addr: u64,
    },
    /// The block at `old` became a block of `size` bytes at `new`, which may be `old`.
    Reallocate {
        old: u64,
        new: u64,
        size: u64,
        align: u64,
    },
}

impl From<Event> for Record {
    fn from(event: Event) -> Record {
        if event.is_free() {
            Record::Free { addr: event.addr as u64 }
        } else {
            Record::Allocate { addr: event.addr as u64, size: event.size as u64, align: event.align as u64 }
        }
    }
}

/// What was wrong with a trace file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Problem {
    /// The file doesn't start with `gtt 1` or `GTT\x01`.
    NotATrace,
    /// A line that is none of `a`, `f`, `r`, `end` or a comment, or has the wrong fields.
    BadRecord,
    /// An alignment that isn't a power of two.
    BadAlignment,
    /// There is no `end`: the file was cut short.
    NoEnd,
    /// There is a record after `end`.
    AfterEnd,
    /// `end` gives a different number of records than the file holds.
    Count { said: u64, found: u64 },
    /// `end` gives a different checksum than the records have.
    Checksum { said: u64, found: u64 },
}

/// A [`Problem`], and where it is: the line of a text file, counted from 1, or the offset
/// of the byte in a binary one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Error {
    pub at: usize,
    pub problem: Problem,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = self.at;
        match self.problem {
            Problem::NotATrace => write!(f, "not a trace: it doesn't start with `gtt 1` or with the bytes GTT 01"),
            Problem::BadRecord => write!(f, "at {at}: not a record"),
            Problem::BadAlignment => write!(f, "at {at}: an alignment that isn't a power of two"),
            Problem::NoEnd => write!(f, "at {at}: the trace stops without an `end`: it was cut short"),
            Problem::AfterEnd => write!(f, "at {at}: a record after `end`"),
            Problem::Count { said, found } => {
                write!(f, "at {at}: `end` says {said} records, and there are {found}")
            }
            Problem::Checksum { said, found } => {
                write!(f, "at {at}: `end` says the checksum is {said:016x}, and the records' is {found:016x}")
            }
        }
    }
}

/// The checksum of a trace, built up a record at a time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Checksum(pub u64);

impl Default for Checksum {
    fn default() -> Checksum {
        Checksum(0xcbf2_9ce4_8422_2325) // FNV's offset basis
    }
}

impl Checksum {
    fn byte(&mut self, byte: u8) {
        self.0 = (self.0 ^ byte as u64).wrapping_mul(0x0000_0100_0000_01b3); // FNV's prime
    }

    fn field(&mut self, field: u64) {
        field.to_le_bytes().into_iter().for_each(|byte| self.byte(byte));
    }

    pub fn add(&mut self, record: &Record) {
        match *record {
            Record::Allocate { addr, size, align } => {
                self.byte(b'a');
                [addr, size, align].into_iter().for_each(|field| self.field(field));
            }
            Record::Free { addr } => {
                self.byte(b'f');
                self.field(addr);
            }
            Record::Reallocate { old, new, size, align } => {
                self.byte(b'r');
                [old, new, size, align].into_iter().for_each(|field| self.field(field));
            }
        }
    }
}

/// The checksum of `records`.
pub fn checksum(records: &[Record]) -> u64 {
    let mut sum = Checksum::default();
    records.iter().for_each(|record| sum.add(record));
    sum.0
}

/// Writes the text form a record at a time, so that a trace can be written without being
/// held in memory twice: the kernel writes one to its serial port this way.
pub struct TextWriter<W: fmt::Write> {
    out: W,
    count: u64,
    sum: Checksum,
}

impl<W: fmt::Write> TextWriter<W> {
    /// Write the first line.
    pub fn new(mut out: W) -> Result<TextWriter<W>, fmt::Error> {
        out.write_str("gtt 1\n")?;
        Ok(TextWriter { out, count: 0, sum: Checksum::default() })
    }

    /// Write a comment. `text` must be one line.
    pub fn comment(&mut self, text: &str) -> fmt::Result {
        writeln!(self.out, "# {text}")
    }

    pub fn record(&mut self, record: &Record) -> fmt::Result {
        self.count += 1;
        self.sum.add(record);
        match *record {
            Record::Allocate { addr, size, align } => writeln!(self.out, "a {addr:x} {size} {align}"),
            Record::Free { addr } => writeln!(self.out, "f {addr:x}"),
            Record::Reallocate { old, new, size, align } => writeln!(self.out, "r {old:x} {new:x} {size} {align}"),
        }
    }

    /// Write the last line, and return how many records were written and their checksum.
    pub fn end(mut self) -> Result<(u64, u64), fmt::Error> {
        writeln!(self.out, "end {} {:016x}", self.count, self.sum.0)?;
        Ok((self.count, self.sum.0))
    }
}

/// Write `records` to `out` in the text form.
pub fn write_text(records: &[Record], out: &mut impl fmt::Write) -> fmt::Result {
    let mut writer = TextWriter::new(out)?;
    for record in records {
        writer.record(record)?;
    }
    writer.end().map(|_| ())
}

/// Read the text form.
pub fn read_text(text: &str) -> Result<Vec<Record>, Error> {
    let lines = text.lines().enumerate().map(|(i, line)| (i + 1, line.trim_ascii()));
    let mut lines = lines.filter(|(_, line)| !line.is_empty() && !line.starts_with('#'));
    match lines.next() {
        Some((_, "gtt 1")) => {}
        _ => return Err(Error { at: 1, problem: Problem::NotATrace }),
    }
    let mut records = Vec::new();
    let mut sum = Checksum::default();
    let mut last = 1;
    while let Some((at, line)) = lines.next() {
        last = at;
        let bad = Error { at, problem: Problem::BadRecord };
        let mut fields = line.split_ascii_whitespace();
        let kind = fields.next().ok_or(bad)?;
        let mut number = |radix| number(fields.next(), radix).ok_or(bad);
        let record = match kind {
            "a" => Record::Allocate { addr: number(16)?, size: number(10)?, align: number(10)? },
            "f" => Record::Free { addr: number(16)? },
            "r" => Record::Reallocate { old: number(16)?, new: number(16)?, size: number(10)?, align: number(10)? },
            "end" => {
                let said = (number(10)?, number(16)?);
                if fields.next().is_some() {
                    return Err(bad);
                }
                if let Some((at, _)) = lines.next() {
                    return Err(Error { at, problem: Problem::AfterEnd });
                }
                return ended(records, sum, said, at);
            }
            _ => return Err(bad),
        };
        if fields.next().is_some() {
            return Err(bad);
        }
        push(&mut records, &mut sum, record, at)?;
    }
    Err(Error { at: last, problem: Problem::NoEnd })
}

/// A field of a line: digits of `radix` and nothing else, not even a sign.
fn number(field: Option<&str>, radix: u32) -> Option<u64> {
    let field = field.filter(|field| field.bytes().all(|byte| (byte as char).is_digit(radix)))?;
    u64::from_str_radix(field, radix).ok()
}

fn push(records: &mut Vec<Record>, sum: &mut Checksum, record: Record, at: usize) -> Result<(), Error> {
    if let Record::Allocate { align, .. } | Record::Reallocate { align, .. } = record
        && !align.is_power_of_two()
    {
        return Err(Error { at, problem: Problem::BadAlignment });
    }
    sum.add(&record);
    records.push(record);
    Ok(())
}

/// Check what an `end` said against what was read.
fn ended(records: Vec<Record>, sum: Checksum, said: (u64, u64), at: usize) -> Result<Vec<Record>, Error> {
    if said.0 != records.len() as u64 {
        return Err(Error { at, problem: Problem::Count { said: said.0, found: records.len() as u64 } });
    }
    if said.1 != sum.0 {
        return Err(Error { at, problem: Problem::Checksum { said: said.1, found: sum.0 } });
    }
    Ok(records)
}

/// What a binary trace starts with.
pub const MAGIC: [u8; 4] = *b"GTT\x01";

const FREED: u8 = 0;
const ALLOCATED: u8 = 1;
const REALLOCATED: u8 = 2;
const END: u8 = 3;

fn varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push(n as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

/// The difference `to - from`, zigzagged.
fn zigzag(out: &mut Vec<u8>, from: u64, to: u64) {
    let delta = to.wrapping_sub(from) as i64;
    varint(out, ((delta << 1) ^ (delta >> 63)) as u64);
}

/// The binary form of `records`.
///
/// # Panics
/// If an alignment isn't a power of two.
pub fn write_binary(records: &[Record]) -> Vec<u8> {
    let mut out = Vec::from(MAGIC);
    let mut last = 0;
    let log = |align: u64| {
        assert!(align.is_power_of_two(), "an alignment that isn't a power of two");
        (align.trailing_zeros() as u8) << 2
    };
    for record in records {
        match *record {
            Record::Allocate { addr, size, align } => {
                out.push(ALLOCATED | log(align));
                zigzag(&mut out, last, addr);
                varint(&mut out, size);
                last = addr;
            }
            Record::Free { addr } => {
                out.push(FREED);
                zigzag(&mut out, last, addr);
                last = addr;
            }
            Record::Reallocate { old, new, size, align } => {
                out.push(REALLOCATED | log(align));
                zigzag(&mut out, last, old);
                zigzag(&mut out, old, new);
                varint(&mut out, size);
                last = new;
            }
        }
    }
    out.push(END);
    varint(&mut out, records.len() as u64);
    out.extend_from_slice(&checksum(records).to_le_bytes());
    out
}

/// A binary trace being read: the bytes, and how far into them we are.
struct Bytes<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Bytes<'_> {
    fn error(&self, problem: Problem) -> Error {
        Error { at: self.at, problem }
    }

    fn byte(&mut self) -> Result<u8, Error> {
        let byte = *self.bytes.get(self.at).ok_or(self.error(Problem::NoEnd))?;
        self.at += 1;
        Ok(byte)
    }

    fn varint(&mut self) -> Result<u64, Error> {
        let mut n = 0;
        for shift in (0..64).step_by(7) {
            let byte = self.byte()?;
            n |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(n);
            }
        }
        Err(self.error(Problem::BadRecord))
    }

    /// The address that is a zigzagged difference away from `from`.
    fn zigzag(&mut self, from: u64) -> Result<u64, Error> {
        let n = self.varint()?;
        Ok(from.wrapping_add((n >> 1) ^ (n & 1).wrapping_neg()))
    }
}

/// Read the binary form.
pub fn read_binary(bytes: &[u8]) -> Result<Vec<Record>, Error> {
    if !bytes.starts_with(&MAGIC) {
        return Err(Error { at: 0, problem: Problem::NotATrace });
    }
    let mut bytes = Bytes { bytes, at: MAGIC.len() };
    let mut records = Vec::new();
    let mut sum = Checksum::default();
    let mut last = 0;
    loop {
        let at = bytes.at;
        let first = bytes.byte()?;
        let align = 1u64.checked_shl((first >> 2) as u32).ok_or(Error { at, problem: Problem::BadAlignment })?;
        let record = match first & 3 {
            ALLOCATED => {
                last = bytes.zigzag(last)?;
                Record::Allocate { addr: last, size: bytes.varint()?, align }
            }
            FREED if first == FREED => {
                last = bytes.zigzag(last)?;
                Record::Free { addr: last }
            }
            REALLOCATED => {
                let old = bytes.zigzag(last)?;
                last = bytes.zigzag(old)?;
                Record::Reallocate { old, new: last, size: bytes.varint()?, align }
            }
            END if first == END => {
                let count = bytes.varint()?;
                let mut said = [0; 8];
                for byte in &mut said {
                    *byte = bytes.byte()?;
                }
                if bytes.at != bytes.bytes.len() {
                    return Err(bytes.error(Problem::AfterEnd));
                }
                return ended(records, sum, (count, u64::from_le_bytes(said)), at);
            }
            _ => return Err(Error { at, problem: Problem::BadRecord }),
        };
        push(&mut records, &mut sum, record, at)?;
    }
}

/// Read a trace in either form: the first bytes say which.
pub fn read(bytes: &[u8]) -> Result<Vec<Record>, Error> {
    if bytes.starts_with(&MAGIC) {
        return read_binary(bytes);
    }
    read_text(core::str::from_utf8(bytes).map_err(|e| Error { at: e.valid_up_to(), problem: Problem::NotATrace })?)
}

/// What [`events`] made of a trace's records.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Events {
    pub events: Vec<Event>,
    /// Reallocations, each of which became a free and an allocation.
    pub reallocations: usize,
    /// Of those, the ones whose block stayed where it was.
    pub in_place: usize,
    /// Allocations of no bytes, which became allocations of one byte.
    pub empty: usize,
    /// Allocations of 4 GiB or more, which an [`Event`] can't hold, left out with their frees.
    pub too_large: usize,
}

/// The events a replay needs, from the records of a file.
///
/// A reallocation becomes a free of the old block and then an allocation of the new one. An
/// [`Event`] whose size is 0 is a free, so an allocation of no bytes becomes an allocation
/// of one byte: the program was given an address, and will free it.
pub fn events(records: &[Record]) -> Events {
    let mut out = Events::default();
    let mut left_out = Vec::new(); // the addresses of blocks too large for an event
    for record in records {
        let (free, allocate) = match *record {
            Record::Allocate { addr, size, align } => (None, Some((addr, size, align))),
            Record::Free { addr } => (Some(addr), None),
            Record::Reallocate { old, new, size, align } => {
                out.reallocations += 1;
                out.in_place += (old == new) as usize;
                (Some(old), Some((new, size, align)))
            }
        };
        if let Some(addr) = free {
            match left_out.iter().position(|&large| large == addr) {
                Some(i) => drop(left_out.swap_remove(i)),
                None => out.events.push(Event::free(addr as usize)),
            }
        }
        if let Some((addr, size, align)) = allocate {
            if size > u32::MAX as u64 || align > u32::MAX as u64 {
                out.too_large += 1;
                left_out.push(addr);
                continue;
            }
            out.empty += (size == 0) as usize;
            out.events.push(Event::allocate(addr as usize, size.max(1) as usize, align as usize));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::{Checksum, Error, Problem, Record, checksum, events, read, read_binary, read_text};
    use super::{TextWriter, write_binary, write_text};
    use crate::trace::{Event, Random};
    use std::string::String;
    use std::vec::Vec;

    const SMALL: [Record; 4] = [
        Record::Allocate { addr: 0x8021_2340, size: 64, align: 8 },
        Record::Allocate { addr: 0x8021_2400, size: 100, align: 16 },
        Record::Reallocate { old: 0x8021_2400, new: 0x8021_2480, size: 128, align: 16 },
        Record::Free { addr: 0x8021_2340 },
    ];

    fn text(records: &[Record]) -> String {
        let mut text = String::new();
        write_text(records, &mut text).unwrap();
        text
    }

    /// Records of every kind, with addresses near one another and far apart.
    fn random_records(count: usize, seed: u64) -> Vec<Record> {
        let mut random = Random(seed);
        let mut addr = 0x7f00_0000_0000u64;
        (0..count)
            .map(|_| {
                let far = random.below(10) == 0;
                let step = random.below(if far { 1 << 40 } else { 4096 }) as u64 * 16;
                addr = if random.below(2) == 0 { addr.wrapping_add(step) } else { addr.wrapping_sub(step) };
                let bits = 1 + random.below(34);
                let (size, align) = (random.below(1 << bits) as u64, 1 << random.below(13));
                match random.below(5) {
                    0 | 1 => Record::Allocate { addr, size, align },
                    2 | 3 => Record::Free { addr },
                    _ => Record::Reallocate { old: addr, new: addr ^ (random.below(1 << 20) as u64), size, align },
                }
            })
            .collect()
    }

    fn problem<T: core::fmt::Debug>(result: Result<T, Error>) -> Problem {
        result.unwrap_err().problem
    }

    #[test]
    fn the_text_form_is_what_the_documentation_says() {
        assert_eq!(
            text(&SMALL),
            "gtt 1\n\
             a 80212340 64 8\n\
             a 80212400 100 16\n\
             r 80212400 80212480 128 16\n\
             f 80212340\n\
             end 4 4c0a14dc9661dbca\n"
        );
        assert_eq!(text(&[]), "gtt 1\nend 0 cbf29ce484222325\n");
    }

    #[test]
    fn the_checksum_is_fnv_1a_over_tags_and_fields() {
        // By hand: FNV-1a of the nine bytes 'f', 1, 0, 0, 0, 0, 0, 0, 0.
        let mut sum = 0xcbf2_9ce4_8422_2325u64;
        for byte in [b'f', 1, 0, 0, 0, 0, 0, 0, 0] {
            sum = (sum ^ byte as u64).wrapping_mul(0x100_0000_01b3);
        }
        assert_eq!(checksum(&[Record::Free { addr: 1 }]), sum);
        assert_ne!(checksum(&SMALL), checksum(&SMALL[..3]));
        // The order of the records is part of it.
        assert_ne!(checksum(&[SMALL[0], SMALL[1]]), checksum(&[SMALL[1], SMALL[0]]));
        assert_eq!(Checksum::default().0, checksum(&[]));
    }

    #[test]
    fn text_reads_back_as_it_was_written() {
        for seed in 1..=5 {
            let records = random_records(3000, seed);
            assert_eq!(read_text(&text(&records)).unwrap(), records);
        }
        assert_eq!(read_text(&text(&[])).unwrap(), []);
    }

    #[test]
    fn binary_reads_back_as_it_was_written() {
        for seed in 1..=5 {
            let records = random_records(3000, seed);
            assert_eq!(read_binary(&write_binary(&records)).unwrap(), records);
        }
        assert_eq!(read_binary(&write_binary(&[])).unwrap(), []);
        let extremes = [
            Record::Allocate { addr: u64::MAX, size: u64::MAX, align: 1 << 63 },
            Record::Free { addr: 0 },
            Record::Reallocate { old: 1 << 63, new: 0, size: 0, align: 1 },
        ];
        assert_eq!(read_binary(&write_binary(&extremes)).unwrap(), extremes);
        assert_eq!(read_text(&text(&extremes)).unwrap(), extremes);
    }

    #[test]
    fn the_binary_form_is_what_the_documentation_says() {
        let records = [
            Record::Allocate { addr: 0x100, size: 300, align: 16 },
            Record::Free { addr: 0xf0 },
            Record::Reallocate { old: 0xf0, new: 0xf8, size: 5, align: 8 },
        ];
        let mut expected = Vec::from(*b"GTT\x01");
        expected.extend([1 | 4 << 2, 0x80, 0x04, 0xac, 0x02]); // +0x100 zigzags to 0x200; 300
        expected.extend([0, 0x1f]); // -0x10 zigzags to 0x1f
        expected.extend([2 | 3 << 2, 0x00, 0x10, 0x05]); // +0, then +8
        expected.extend([3, 3]);
        expected.extend(checksum(&records).to_le_bytes());
        assert_eq!(write_binary(&records), expected);
    }

    #[test]
    fn both_forms_are_told_apart_by_their_first_bytes() {
        let records = random_records(100, 9);
        assert_eq!(read(text(&records).as_bytes()).unwrap(), records);
        assert_eq!(read(&write_binary(&records)).unwrap(), records);
        assert_eq!(problem(read(b"hello\n")), Problem::NotATrace);
        assert_eq!(problem(read(b"")), Problem::NotATrace);
        assert_eq!(problem(read(b"gtt 2\nend 0 cbf29ce484222325\n")), Problem::NotATrace);
        assert_eq!(problem(read(b"\xff\xfe")), Problem::NotATrace);
    }

    const SMALL_PAIR: [Record; 2] = [Record::Allocate { addr: 0x10, size: 5, align: 8 }, Record::Free { addr: 0x10 }];

    #[test]
    fn a_serial_console_s_line_endings_and_comments_are_read() {
        let sum = checksum(&SMALL_PAIR);
        let text = std::format!(
            "\r\n# from the kernel\r\ngtt 1\r\n  a 10 5 8\r\n\r\n# half way\r\nf 10\r\nend 2 {sum:016x}\r\n# done\r\n"
        );
        assert_eq!(read_text(&text).unwrap(), SMALL_PAIR);
    }

    #[test]
    fn a_trace_cut_short_is_refused() {
        let whole = text(&SMALL);
        let cut = &whole[..whole.find("end").unwrap()];
        assert_eq!(read_text(cut), Err(Error { at: 5, problem: Problem::NoEnd }));
        assert_eq!(problem(read_text("gtt 1\n")), Problem::NoEnd);
        let whole = write_binary(&SMALL);
        for len in 4..whole.len() {
            assert_eq!(problem(read_binary(&whole[..len])), Problem::NoEnd, "cut to {len} bytes");
        }
    }

    #[test]
    fn a_missing_line_is_caught_by_the_count() {
        let whole = text(&SMALL);
        let without = whole.replace("a 80212400 100 16\n", "");
        assert_eq!(read_text(&without), Err(Error { at: 5, problem: Problem::Count { said: 4, found: 3 } }));
    }

    #[test]
    fn a_changed_digit_is_caught_by_the_checksum() {
        let whole = text(&SMALL);
        for (from, to) in [("a 80212340 64 8", "a 80212340 65 8"), ("f 80212340", "f 80212348"), ("128 16", "128 32")] {
            let changed = whole.replace(from, to);
            assert!(matches!(problem(read_text(&changed)), Problem::Checksum { .. }), "{from} to {to}");
        }
        // Two lines swapped: the count is right, and the checksum isn't.
        let swapped = whole.replace("a 80212340 64 8\na 80212400 100 16\n", "a 80212400 100 16\na 80212340 64 8\n");
        assert!(matches!(problem(read_text(&swapped)), Problem::Checksum { .. }));
        // In the binary form, every byte matters: flip each in turn.
        let whole = write_binary(&SMALL);
        for i in 0..whole.len() {
            let mut changed = whole.clone();
            changed[i] ^= 0x04;
            assert!(read_binary(&changed).is_err(), "byte {i} flipped went unnoticed");
        }
    }

    #[test]
    fn lines_that_are_not_records_are_refused() {
        for (line, at_fault) in [
            ("x 10", Problem::BadRecord),
            ("a 10 5", Problem::BadRecord),
            ("a 10 5 8 9", Problem::BadRecord),
            ("a 1g 5 8", Problem::BadRecord),
            ("a 10 0x5 8", Problem::BadRecord),
            ("a 10 -5 8", Problem::BadRecord),
            ("a +10 5 8", Problem::BadRecord),
            ("a 10 +5 8", Problem::BadRecord),
            ("f", Problem::BadRecord),
            ("f 10 20", Problem::BadRecord),
            ("r 10 20 5", Problem::BadRecord),
            ("end 1", Problem::BadRecord),
            ("a 10 5 12", Problem::BadAlignment),
            ("a 10 5 0", Problem::BadAlignment),
            ("r 10 20 5 3", Problem::BadAlignment),
        ] {
            let text = std::format!("gtt 1\n{line}\nend 1 0\n");
            assert_eq!(read_text(&text), Err(Error { at: 2, problem: at_fault }), "{line}");
        }
        let after = text(&SMALL) + "f 80212480\n";
        assert_eq!(read_text(&after), Err(Error { at: 7, problem: Problem::AfterEnd }));
        let mut after = write_binary(&SMALL);
        after.push(0);
        assert_eq!(problem(read_binary(&after)), Problem::AfterEnd);
    }

    #[test]
    fn a_writer_counts_what_it_wrote() {
        let mut out = String::new();
        let mut writer = TextWriter::new(&mut out).unwrap();
        writer.comment("two records").unwrap();
        SMALL_PAIR.iter().for_each(|record| writer.record(record).unwrap());
        assert_eq!(writer.end().unwrap(), (2, checksum(&SMALL_PAIR)));
        assert_eq!(read_text(&out).unwrap(), SMALL_PAIR);
    }

    #[test]
    fn events_become_records_and_back() {
        let trace = [Event::allocate(0xa0, 100, 8), Event::allocate(0xb0, 50, 16), Event::free(0xa0)];
        let records: Vec<Record> = trace.iter().map(|&event| event.into()).collect();
        assert_eq!(records[2], Record::Free { addr: 0xa0 });
        let back = events(&records);
        assert_eq!(back.events, trace);
        assert_eq!((back.reallocations, back.empty, back.too_large), (0, 0, 0));
    }

    #[test]
    fn a_reallocation_is_a_free_and_then_an_allocation() {
        let made = events(&SMALL);
        assert_eq!(
            made.events,
            [
                Event::allocate(0x8021_2340, 64, 8),
                Event::allocate(0x8021_2400, 100, 16),
                Event::free(0x8021_2400),
                Event::allocate(0x8021_2480, 128, 16),
                Event::free(0x8021_2340),
            ]
        );
        assert_eq!((made.reallocations, made.in_place), (1, 0));
        // In place: the free comes first, so the address names the new block afterwards.
        let grown = [
            Record::Allocate { addr: 0x10, size: 8, align: 16 },
            Record::Reallocate { old: 0x10, new: 0x10, size: 24, align: 16 },
            Record::Free { addr: 0x10 },
        ];
        let made = events(&grown);
        assert_eq!(
            made.events,
            [Event::allocate(0x10, 8, 16), Event::free(0x10), Event::allocate(0x10, 24, 16), Event::free(0x10)]
        );
        assert_eq!((made.reallocations, made.in_place), (1, 1));
    }

    #[test]
    fn an_allocation_of_no_bytes_is_not_mistaken_for_a_free() {
        let records = [Record::Allocate { addr: 0x10, size: 0, align: 16 }, Record::Free { addr: 0x10 }];
        let made = events(&records);
        assert_eq!(made.events, [Event::allocate(0x10, 1, 16), Event::free(0x10)]);
        assert_eq!(made.empty, 1);
        assert!(!made.events[0].is_free());
    }

    #[test]
    fn a_block_too_large_for_an_event_is_left_out_with_its_free() {
        let records = [
            Record::Allocate { addr: 0x10, size: 1 << 32, align: 16 },
            Record::Allocate { addr: 0x20, size: 8, align: 16 },
            Record::Free { addr: 0x10 },
            Record::Allocate { addr: 0x10, size: 8, align: 16 },
            Record::Free { addr: 0x10 },
        ];
        let made = events(&records);
        assert_eq!(made.events, [Event::allocate(0x20, 8, 16), Event::allocate(0x10, 8, 16), Event::free(0x10)]);
        assert_eq!(made.too_large, 1);
    }
}
