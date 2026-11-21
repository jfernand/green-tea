//! The HID boot keyboard report: eight bytes, of which the first says which modifiers are
//! down and the last six which keys. A key is typed when it appears in a report and wasn't
//! in the one before.

/// Report byte 2's value when too many keys are down to list.
const ROLLOVER: u8 = 1;
const CTRL: u8 = 0b0001_0001;
const SHIFT: u8 = 0b0010_0010;

/// Usages 4 (a) to 56 (/), on a US layout. 0 is nothing.
const PLAIN: &[u8; 53] = b"abcdefghijklmnopqrstuvwxyz1234567890\r\x1b\x7f\t -=[]\\\0;'`,./";
const SHIFTED: &[u8; 53] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ!@#$%^&*()\r\x1b\x7f\t _+{}|\0:\"~<>?";

/// Remembers the last report, to tell a new key from a held one.
#[derive(Default)]
pub struct Decoder {
    held: [u8; 6],
}

impl Decoder {
    pub const fn new() -> Decoder {
        Decoder { held: [0; 6] }
    }

    /// Take one report, and call `typed` with each character it adds.
    pub fn feed(&mut self, report: &[u8; 8], mut typed: impl FnMut(u8)) {
        let keys: [u8; 6] = [report[2], report[3], report[4], report[5], report[6], report[7]];
        if keys.contains(&ROLLOVER) {
            return; // says nothing about which keys are down: keep what we knew
        }
        for &key in keys
            .iter()
            .filter(|&&k| {
                k != 0
                    && !self
                        .held
                        .contains(&k)
            })
        {
            if let Some(c) = character(report[0], key) {
                typed(c);
            }
        }
        self.held = keys;
    }
}

fn character(modifiers: u8, key: u8) -> Option<u8> {
    let table = if modifiers & SHIFT != 0 { SHIFTED } else { PLAIN };
    let c = *table.get(usize::from(key.checked_sub(4)?))?;
    match c {
        0 => None,
        c if modifiers & CTRL != 0 && c.is_ascii_alphabetic() => Some(c.to_ascii_uppercase() - b'@'),
        c => Some(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    fn typed(reports: &[[u8; 8]]) -> Vec<u8> {
        let mut d = Decoder::new();
        let mut out = Vec::new();
        reports
            .iter()
            .for_each(|r| d.feed(r, |c| out.push(c)));
        out
    }

    const fn keys(modifiers: u8, a: u8, b: u8) -> [u8; 8] {
        [modifiers, 0, a, b, 0, 0, 0, 0]
    }

    #[test]
    fn a_held_key_types_once() {
        assert_eq!(typed(&[keys(0, 4, 0), keys(0, 4, 0), keys(0, 0, 0)]), b"a");
    }

    #[test]
    fn pressing_it_again_types_again() {
        assert_eq!(typed(&[keys(0, 4, 0), keys(0, 0, 0), keys(0, 4, 0)]), b"aa");
    }

    #[test]
    fn two_keys_in_one_report() {
        assert_eq!(typed(&[keys(0, 4, 5)]), b"ab");
    }

    #[test]
    fn a_second_key_while_the_first_is_held() {
        assert_eq!(typed(&[keys(0, 4, 0), keys(0, 4, 5)]), b"ab");
    }

    #[test]
    fn either_shift_key() {
        assert_eq!(typed(&[keys(0x02, 4, 0), keys(0, 0, 0), keys(0x20, 30, 0)]), b"A!");
    }

    #[test]
    fn ctrl_c_is_03() {
        assert_eq!(typed(&[keys(0x01, 6, 0)]), [3]);
    }

    #[test]
    fn enter_backspace_tab_and_space() {
        assert_eq!(typed(&[keys(0, 40, 0), keys(0, 42, 0), keys(0, 43, 0), keys(0, 44, 0)]), b"\r\x7f\t ");
    }

    #[test]
    fn punctuation_past_the_gap_in_the_usages() {
        assert_eq!(typed(&[keys(0, 51, 0), keys(0, 56, 0)]), b";/");
    }

    #[test]
    fn a_rollover_report_types_nothing_and_forgets_nothing() {
        assert_eq!(typed(&[keys(0, 4, 0), [0, 0, 1, 1, 1, 1, 1, 1], keys(0, 4, 0)]), b"a");
    }

    #[test]
    fn a_key_past_the_table_is_nothing() {
        assert_eq!(typed(&[keys(0, 200, 0)]), b"");
    }
}
