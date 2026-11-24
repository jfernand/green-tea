//! The keyboard lights of a Clevo laptop. Its embedded controller (EC) has a five-byte
//! mailbox: we write what to set and the values, then a command byte that tells the EC to
//! act. The laptop's firmware does the same in AML (the report's appendix on `WMBB`).
//!
//! The mailbox's meaning is private to each firmware, and it serves other EC functions
//! too, so [`Lights::new`] refuses any firmware not listed in [`KNOWN_FIRMWARE`].

#![no_std]

use core::fmt;

/// Where the mailbox is in the EC's address space.
const FCMD: u8 = 0xf8;
const FDAT: u8 = 0xf9;
const FBUF: u8 = 0xfa;
const FBF1: u8 = 0xfb;
const FBF2: u8 = 0xfc;
/// The mailbox command that sets a color or the brightness.
const SET_LIGHTS: u8 = 0xca;
/// `FDAT` for the keyboard's color, and for its brightness. The firmware also writes `04`
/// and `05`, for the center and right of a three-zone keyboard. This laptop's keyboard has
/// one zone: `03` colors all of it, and the other two change nothing, so we never send them.
const COLOR: u8 = 0x03;
const BRIGHTNESS: u8 = 0x06;

/// The EC's standard command to write a byte of its address space.
const EC_WRITE: u8 = 0x81;
/// In the status register: the EC hasn't taken the last byte we gave it.
const INPUT_FULL: u8 = 1 << 1;
const WAIT_MS: u32 = 100;
/// The firmware doesn't wait for the EC to act. We leave it this long between commands.
const SETTLE_MS: u64 = 10;

/// The EC's two I/O ports.
pub trait EcPorts {
    fn status(&mut self) -> u8;
    fn command(&mut self, value: u8);
    fn data(&mut self, value: u8);
}

/// What identifies a firmware: its DSDT's table id, length and checksum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Firmware {
    pub table_id: [u8; 8],
    pub length: u32,
    pub checksum: u8,
}

/// The firmwares whose AML we have read: a Clevo NP50PN at BIOS 1.07.12.
pub const KNOWN_FIRMWARE: &[Firmware] = &[Firmware { table_id: *b"ADL-P-M\0", length: 480_126, checksum: 0x97 }];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl Color {
    /// A color's name, or six hexadecimal digits (`ff8000`).
    pub fn parse(text: &str) -> Option<Color> {
        let rgb = match text {
            "red" => 0xff0000,
            "green" => 0x00ff00,
            "blue" => 0x0000ff,
            "white" => 0xffffff,
            "orange" => 0xff8000,
            "yellow" => 0xffff00,
            "cyan" => 0x00ffff,
            "purple" => 0x8000ff,
            "pink" => 0xff0080,
            hex if hex.len() == 6
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit()) =>
            {
                u32::from_str_radix(hex, 16).ok()?
            }
            _ => return None,
        };
        Some(Color { red: (rgb >> 16) as u8, green: (rgb >> 8) as u8, blue: rgb as u8 })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Not a firmware we have read, so we don't know what the mailbox would do.
    UnknownFirmware,
    /// The EC didn't take a byte. The mailbox's command wasn't sent.
    Timeout,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Error::UnknownFirmware => "this firmware isn't one whose lights we know, so nothing was sent",
            Error::Timeout => "the embedded controller didn't answer, so the command wasn't sent",
        })
    }
}

pub struct Lights<P> {
    ports: P,
    delay_ms: fn(u64),
}

impl<P: EcPorts> Lights<P> {
    /// The lights of the machine running `firmware`, if we know that firmware.
    pub fn new(ports: P, delay_ms: fn(u64), firmware: Firmware) -> Result<Lights<P>, Error> {
        if !KNOWN_FIRMWARE.contains(&firmware) {
            return Err(Error::UnknownFirmware);
        }
        Ok(Lights { ports, delay_ms })
    }

    /// The color of the whole keyboard.
    pub fn set_color(&mut self, color: Color) -> Result<(), Error> {
        // The firmware's order: blue, red, green.
        self.mailbox(COLOR, color.blue, color.red, color.green)
    }

    /// The brightness of the whole keyboard. 0 is off.
    pub fn set_brightness(&mut self, brightness: u8) -> Result<(), Error> {
        self.mailbox(BRIGHTNESS, brightness, 0, 0)
    }

    /// Fill the mailbox, and write its command last: that's what makes the EC act.
    fn mailbox(&mut self, what: u8, first: u8, second: u8, third: u8) -> Result<(), Error> {
        self.write(FDAT, what)?;
        self.write(FBUF, first)?;
        self.write(FBF1, second)?;
        self.write(FBF2, third)?;
        self.write(FCMD, SET_LIGHTS)?;
        (self.delay_ms)(SETTLE_MS);
        Ok(())
    }

    /// Write one byte of the EC's address space, by the ACPI specification's protocol.
    fn write(&mut self, address: u8, value: u8) -> Result<(), Error> {
        self.wait()?;
        self.ports
            .command(EC_WRITE);
        self.wait()?;
        self.ports
            .data(address);
        self.wait()?;
        self.ports
            .data(value);
        Ok(())
    }

    fn wait(&mut self) -> Result<(), Error> {
        for _ in 0..WAIT_MS {
            if self
                .ports
                .status()
                & INPUT_FULL
                == 0
            {
                return Ok(());
            }
            (self.delay_ms)(1);
        }
        Err(Error::Timeout)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    /// An EC that decodes the protocol into the writes it receives.
    #[derive(Default)]
    struct Fake {
        writes: Vec<(u8, u8)>,
        address: Option<u8>,
        expecting_address: bool,
        /// Stop taking bytes after this many.
        stuck_after: Option<usize>,
        bytes: usize,
        /// Whether a byte was written while the EC was still busy.
        overrun: bool,
        busy: bool,
    }

    impl EcPorts for &mut Fake {
        fn status(&mut self) -> u8 {
            if self
                .stuck_after
                .is_some_and(|n| self.bytes >= n)
            {
                return INPUT_FULL;
            }
            // Each byte keeps the EC busy for one read of the status.
            let status = if self.busy { INPUT_FULL } else { 0 };
            self.busy = false;
            status
        }
        fn command(&mut self, value: u8) {
            assert_eq!(value, EC_WRITE);
            self.overrun |= self.busy;
            self.busy = true;
            self.bytes += 1;
            self.expecting_address = true;
        }
        fn data(&mut self, value: u8) {
            self.overrun |= self.busy;
            self.busy = true;
            self.bytes += 1;
            if self.expecting_address {
                self.address = Some(value);
                self.expecting_address = false;
            } else {
                self.writes
                    .push((
                        self.address
                            .take()
                            .unwrap(),
                        value,
                    ));
            }
        }
    }

    const KNOWN: Firmware = KNOWN_FIRMWARE[0];

    #[test]
    fn a_color_is_the_five_writes_the_firmware_makes() {
        let mut ec = Fake::default();
        let mut lights = Lights::new(&mut ec, |_| {}, KNOWN).unwrap();
        lights
            .set_color(Color::parse("orange").unwrap())
            .unwrap();
        assert_eq!(ec.writes, [(0xf9, 0x03), (0xfa, 0x00), (0xfb, 0xff), (0xfc, 0x80), (0xf8, 0xca)]);
        assert!(!ec.overrun, "every byte waited for the last");
    }

    #[test]
    fn the_brightness_goes_through_the_same_mailbox() {
        let mut ec = Fake::default();
        let mut lights = Lights::new(&mut ec, |_| {}, KNOWN).unwrap();
        lights
            .set_brightness(0x40)
            .unwrap();
        assert_eq!(ec.writes, [(0xf9, 0x06), (0xfa, 0x40), (0xfb, 0), (0xfc, 0), (0xf8, 0xca)]);
    }

    #[test]
    fn any_other_firmware_is_refused() {
        for other in [
            Firmware { checksum: 0x98, ..KNOWN },
            Firmware { length: KNOWN.length + 1, ..KNOWN },
            Firmware { table_id: *b"ADL-P-X\0", ..KNOWN },
        ] {
            let mut ec = Fake::default();
            assert!(matches!(Lights::new(&mut ec, |_| {}, other), Err(Error::UnknownFirmware)));
            assert_eq!(ec.bytes, 0, "nothing was sent");
        }
    }

    #[test]
    fn a_stuck_ec_never_gets_the_command_byte() {
        // Stop at every point of the 15 bytes before the last write's value.
        for stuck in 0..15 {
            let mut ec = Fake { stuck_after: Some(stuck), ..Fake::default() };
            let mut lights = Lights::new(&mut ec, |_| {}, KNOWN).unwrap();
            assert_eq!(lights.set_brightness(1), Err(Error::Timeout));
            assert!(
                !ec.writes
                    .iter()
                    .any(|w| w.0 == FCMD),
                "stuck after {stuck} bytes"
            );
        }
    }

    #[test]
    fn colors_parse() {
        assert_eq!(Color::parse("ff8000"), Color::parse("orange"));
        assert_eq!(Color::parse("0000FF"), Color::parse("blue"));
        for bad in ["", "mauve", "ff800", "ff80000", "gg0000", "+f8000"] {
            assert_eq!(Color::parse(bad), None, "{bad}");
        }
    }
}
