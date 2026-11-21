//! Just enough ACPI to learn a machine's CPUs and clocks: from the root pointer to the
//! root table, and from there to the MADT (the local APICs), the HPET table and the FADT
//! (the power management timer). Every table's checksum is verified before it is used.

#![no_std]

/// How the tables are read, so that the tests can use a fake memory.
pub trait Memory {
    /// Copy the bytes at physical address `addr`. `false` if they can't be reached.
    fn read(&self, addr: u64, out: &mut [u8]) -> bool;
}

/// Physical memory through an identity map of its first 4 GiB.
pub struct Identity4G;

impl Memory for Identity4G {
    fn read(&self, addr: u64, out: &mut [u8]) -> bool {
        if addr.checked_add(out.len() as u64).is_none_or(|end| end > 1 << 32) {
            return false;
        }
        // SAFETY: the board maps the first 4 GiB, and firmware tables aren't written.
        unsafe { core::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), out.len()) };
        true
    }
}

/// The largest table we read.
const MAX_TABLE: usize = 8192;
const HEADER: usize = 36;

fn sum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |a, b| a.wrapping_add(*b))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The root table: where it is, and how wide its pointers are.
pub struct Acpi<M> {
    memory: M,
    root: u64,
    entry: usize,
}

impl<M: Memory> Acpi<M> {
    /// From the bytes of the root pointer (RSDP).
    pub fn new(memory: M, rsdp: &[u8]) -> Option<Acpi<M>> {
        if rsdp.get(..8)? != b"RSD PTR " || sum(rsdp.get(..20)?) != 0 {
            return None;
        }
        let (root, entry) = if rsdp[15] >= 2 && rsdp.len() >= 36 && sum(&rsdp[..36]) == 0 {
            (u64_at(rsdp, 24)?, 8) // the XSDT
        } else {
            (u64::from(u32_at(rsdp, 16)?), 4) // the RSDT
        };
        Some(Acpi { memory, root, entry })
    }

    /// Read the table at `addr` into `buf`, and return it if its checksum is right.
    fn table<'b>(&self, addr: u64, buf: &'b mut [u8; MAX_TABLE]) -> Option<&'b [u8]> {
        let mut header = [0u8; HEADER];
        if !self.memory.read(addr, &mut header) {
            return None;
        }
        let len = u32_at(&header, 4)? as usize;
        if !(HEADER..=MAX_TABLE).contains(&len) || !self.memory.read(addr, &mut buf[..len]) {
            return None;
        }
        (sum(&buf[..len]) == 0).then_some(&buf[..len])
    }

    /// Where the table with this signature is.
    fn find(&self, signature: &[u8; 4]) -> Option<u64> {
        let mut root = [0u8; MAX_TABLE];
        let root = self.table(self.root, &mut root)?;
        root[HEADER..].chunks_exact(self.entry).find_map(|e| {
            let addr = if self.entry == 8 { u64_at(e, 0)? } else { u64::from(u32_at(e, 0)?) };
            let mut header = [0u8; 4];
            (self.memory.read(addr, &mut header) && &header == signature).then_some(addr)
        })
    }

    /// Put the id of each usable local APIC in `ids`, in the MADT's order, with the boot
    /// CPU's first, and return how many there are. Ids need not be consecutive.
    pub fn apic_ids(&self, boot: u32, ids: &mut [u32]) -> usize {
        const ENABLED: u32 = 1;
        const ONLINE_CAPABLE: u32 = 2;
        let mut buf = [0u8; MAX_TABLE];
        let Some(madt) = self.find(b"APIC").and_then(|a| self.table(a, &mut buf)) else { return 0 };
        let Some(first) = ids.first_mut() else { return 0 };
        *first = boot;
        let mut count = 1;
        let mut at = HEADER + 8;
        while let (Some(&kind), Some(&len)) = (madt.get(at), madt.get(at + 1)) {
            if len < 2 {
                break;
            }
            let found = match kind {
                0 => madt.get(at + 3).map(|&id| u32::from(id)).zip(u32_at(madt, at + 4)),
                9 => u32_at(madt, at + 4).zip(u32_at(madt, at + 8)),
                _ => None,
            };
            if let Some((id, flags)) = found {
                let usable = flags & (ENABLED | ONLINE_CAPABLE) != 0;
                if usable && id != boot && count < ids.len() && !ids[..count].contains(&id) {
                    ids[count] = id;
                    count += 1;
                }
            }
            at += len as usize;
        }
        count
    }

    /// The HPET's address.
    pub fn hpet(&self) -> Option<u64> {
        let mut buf = [0u8; MAX_TABLE];
        let table = self.table(self.find(b"HPET")?, &mut buf)?;
        // A generic address at offset 40: space id 0 is memory, and its address is 4 on.
        (*table.get(40)? == 0).then(|| u64_at(table, 44)).flatten().filter(|&a| a != 0)
    }

    /// The I/O port of the power management timer, which counts at 3.579545 MHz.
    pub fn pm_timer_port(&self) -> Option<u16> {
        let mut buf = [0u8; MAX_TABLE];
        let fadt = self.table(self.find(b"FACP")?, &mut buf)?;
        u32_at(fadt, 76).filter(|&p| p != 0 && p <= 0xffff).map(|p| p as u16)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::collections::BTreeMap;
    use std::vec::Vec;

    #[derive(Default)]
    struct Fake(BTreeMap<u64, Vec<u8>>);

    impl Memory for &Fake {
        fn read(&self, addr: u64, out: &mut [u8]) -> bool {
            self.0.get(&addr).is_some_and(|t| {
                t.len() >= out.len() && {
                    out.copy_from_slice(&t[..out.len()]);
                    true
                }
            })
        }
    }

    fn table(signature: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut t = std::vec![0u8; HEADER];
        t[..4].copy_from_slice(signature);
        t.extend(body);
        let len = t.len() as u32;
        t[4..8].copy_from_slice(&len.to_le_bytes());
        t[9] = 0u8.wrapping_sub(sum(&t));
        t
    }

    fn rsdp_v2(xsdt: u64) -> Vec<u8> {
        let mut r = std::vec![0u8; 36];
        r[..8].copy_from_slice(b"RSD PTR ");
        r[15] = 2;
        r[24..32].copy_from_slice(&xsdt.to_le_bytes());
        r[8] = 0u8.wrapping_sub(sum(&r[..20]));
        r[32] = 0u8.wrapping_sub(sum(&r));
        r
    }

    fn with(tables: &[(u64, Vec<u8>)]) -> Fake {
        let mut fake = Fake::default();
        let mut pointers = Vec::new();
        for (addr, t) in tables {
            pointers.extend(addr.to_le_bytes());
            fake.0.insert(*addr, t.clone());
        }
        fake.0.insert(0x1000, table(b"XSDT", &pointers));
        fake
    }

    fn lapic(id: u8, flags: u32) -> Vec<u8> {
        let mut e = std::vec![0, 8, id, id];
        e.extend(flags.to_le_bytes());
        e
    }

    fn madt(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut body = std::vec![0u8; 8];
        for e in entries {
            body.extend(e);
        }
        table(b"APIC", &body)
    }

    #[test]
    fn sparse_apic_ids_keep_their_order_with_the_boot_cpu_first() {
        let m = madt(&[lapic(0, 1), lapic(1, 1), lapic(8, 1), lapic(9, 1)]);
        let fake = with(&[(0x2000, m)]);
        let acpi = Acpi::new(&fake, &rsdp_v2(0x1000)).unwrap();
        let mut ids = [0; 8];
        assert_eq!(acpi.apic_ids(8, &mut ids), 4);
        assert_eq!(ids[..4], [8, 0, 1, 9]);
    }

    #[test]
    fn a_disabled_cpu_is_left_out() {
        let fake = with(&[(0x2000, madt(&[lapic(0, 1), lapic(2, 0), lapic(4, 1)]))]);
        let acpi = Acpi::new(&fake, &rsdp_v2(0x1000)).unwrap();
        let mut ids = [0; 8];
        assert_eq!(acpi.apic_ids(0, &mut ids), 2);
        assert_eq!(ids[..2], [0, 4]);
    }

    #[test]
    fn x2apic_entries_are_read_too() {
        let mut x2 = std::vec![9, 16, 0, 0];
        x2.extend(300u32.to_le_bytes());
        x2.extend(1u32.to_le_bytes());
        x2.extend(0u32.to_le_bytes());
        let fake = with(&[(0x2000, madt(&[lapic(0, 1), x2]))]);
        let acpi = Acpi::new(&fake, &rsdp_v2(0x1000)).unwrap();
        let mut ids = [0; 8];
        assert_eq!(acpi.apic_ids(0, &mut ids), 2);
        assert_eq!(ids[1], 300);
    }

    #[test]
    fn no_more_ids_than_room() {
        let fake = with(&[(0x2000, madt(&[lapic(0, 1), lapic(1, 1), lapic(2, 1)]))]);
        let acpi = Acpi::new(&fake, &rsdp_v2(0x1000)).unwrap();
        let mut ids = [0; 2];
        assert_eq!(acpi.apic_ids(0, &mut ids), 2);
    }

    #[test]
    fn finds_the_hpet_and_the_pm_timer() {
        let mut hpet = std::vec![0u8; 20];
        hpet[8..16].copy_from_slice(&0xfed0_0000u64.to_le_bytes());
        let mut fadt = std::vec![0u8; 80];
        fadt[40..44].copy_from_slice(&0x608u32.to_le_bytes());
        let fake = with(&[(0x2000, table(b"HPET", &hpet)), (0x3000, table(b"FACP", &fadt))]);
        let acpi = Acpi::new(&fake, &rsdp_v2(0x1000)).unwrap();
        assert_eq!(acpi.hpet(), Some(0xfed0_0000));
        assert_eq!(acpi.pm_timer_port(), Some(0x608));
    }

    #[test]
    fn a_machine_without_an_hpet_says_so() {
        let fake = with(&[(0x2000, madt(&[lapic(0, 1)]))]);
        assert_eq!(Acpi::new(&fake, &rsdp_v2(0x1000)).unwrap().hpet(), None);
    }

    #[test]
    fn a_table_with_a_bad_checksum_is_ignored() {
        let mut m = madt(&[lapic(0, 1), lapic(1, 1)]);
        m[9] ^= 1;
        let fake = with(&[(0x2000, m)]);
        let mut ids = [0; 8];
        assert_eq!(Acpi::new(&fake, &rsdp_v2(0x1000)).unwrap().apic_ids(0, &mut ids), 0);
    }

    #[test]
    fn a_bad_root_pointer_is_refused() {
        let mut r = rsdp_v2(0x1000);
        r[0] = b'X';
        assert!(Acpi::new(&Fake::default(), &r).is_none());
    }
}
