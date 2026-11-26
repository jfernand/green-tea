//! The i8042, the controller behind a PS/2 keyboard, which is how most laptops' built-in
//! keyboards still appear. Polled: [`Keyboard::getc`] looks for a byte on port 0x60. The
//! controller translates the keyboard's codes to scan code set 1, which [`Decoder`] turns
//! into ASCII on a US layout.

#![no_std]

/// Turns scan codes (set 1) into ASCII, tracking the modifier keys.
#[derive(Default)]
pub struct Decoder {
    shift: bool,
    ctrl: bool,
    extended: bool,
}

/// A byte read from the shared controller output buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawInput {
    pub code: u8,
    pub auxiliary: bool,
}

const RELEASE: u8 = 0x80;
const EXTENDED: u8 = 0xe0;
const LEFT_SHIFT: u8 = 0x2a;
const RIGHT_SHIFT: u8 = 0x36;
const CTRL: u8 = 0x1d;

/// What each make code types, unshifted and shifted. 0 is nothing.
const PLAIN: &[u8; 58] = b"\0\x1b1234567890-=\x7f\tqwertyuiop[]\r\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
const SHIFTED: &[u8; 58] = b"\0\x1b!@#$%^&*()_+\x7f\tQWERTYUIOP{}\r\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";

impl Decoder {
    pub const fn new() -> Decoder {
        Decoder { shift: false, ctrl: false, extended: false }
    }

    /// Take one byte from the keyboard, and return the character it completes, if any.
    pub fn feed(&mut self, code: u8) -> Option<u8> {
        if code == EXTENDED {
            self.extended = true;
            return None;
        }
        let extended = core::mem::take(&mut self.extended);
        let (key, pressed) = (code & !RELEASE, code & RELEASE == 0);
        match key {
            CTRL => self.ctrl = pressed, // the right one is the same code, extended
            // An extended 0x2a or 0x36 is a fake shift some keys send around themselves.
            LEFT_SHIFT | RIGHT_SHIFT if !extended => self.shift = pressed,
            _ if pressed && !extended => return self.character(key),
            _ => {}
        }
        None
    }

    fn character(&self, key: u8) -> Option<u8> {
        let table = if self.shift { SHIFTED } else { PLAIN };
        let c = *table.get(key as usize)?;
        match c {
            0 => None,
            c if self.ctrl && c.is_ascii_alphabetic() => Some(c.to_ascii_uppercase() - b'@'),
            c => Some(c),
        }
    }
}

#[cfg(target_arch = "x86_64")]
pub use port::Keyboard;

#[cfg(target_arch = "x86_64")]
mod port {
    use super::Decoder;
    use core::arch::asm;

    const DATA: u16 = 0x60;
    const STATUS: u16 = 0x64;
    const OUTPUT_FULL: u8 = 1 << 0;
    /// The byte waiting is from the mouse (or a touchpad).
    const FROM_AUX: u8 = 1 << 5;

    fn inb(port: u16) -> u8 {
        let value: u8;
        // SAFETY: reading the controller's ports touches nothing but the controller.
        unsafe { asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack)) };
        value
    }

    fn outb(port: u16, value: u8) {
        // SAFETY: only the controller's own ports, with commands it documents.
        unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack)) };
    }

    /// Wait for the status bit to read `set`. `false` if the controller never gets there.
    fn wait(bit: u8, set: bool) -> bool {
        (0..100_000).any(|_| (inb(STATUS) & bit != 0) == set)
    }

    /// The keyboard on the controller.
    pub struct Keyboard(Decoder);

    impl Default for Keyboard {
        fn default() -> Keyboard {
            Keyboard::new()
        }
    }

    impl Keyboard {
        pub const fn new() -> Keyboard {
            Keyboard(Decoder::new())
        }

        /// Have the controller raise IRQ 1 for the keyboard and IRQ 12 for a mouse. The mouse's
        /// is on because its bytes share the one output buffer: one left unread there
        /// would block the keyboard, and no keyboard interrupt would say so. `false` if there
        /// is no controller, or it doesn't answer.
        pub fn enable_interrupts(&mut self) -> bool {
            const INPUT_FULL: u8 = 1 << 1;
            const READ_CONFIG: u8 = 0x20;
            const WRITE_CONFIG: u8 = 0x60;
            const IRQ1_AND_IRQ12: u8 = 0b11;
            if inb(STATUS) == 0xff {
                return false;
            }
            // Anything waiting would be taken for the configuration byte.
            for _ in 0..32 {
                if inb(STATUS) & OUTPUT_FULL == 0 {
                    break;
                }
                inb(DATA);
            }
            if !wait(INPUT_FULL, false) {
                return false;
            }
            outb(STATUS, READ_CONFIG);
            if !wait(OUTPUT_FULL, true) {
                return false;
            }
            let config = inb(DATA);
            if !wait(INPUT_FULL, false) {
                return false;
            }
            outb(STATUS, WRITE_CONFIG);
            if !wait(INPUT_FULL, false) {
                return false;
            }
            outb(DATA, config | IRQ1_AND_IRQ12);
            true
        }

        /// The next character typed, if one has arrived.
        pub fn getc(&mut self) -> Option<u8> {
            loop {
                let input = self.read_raw()?;
                if let Some(c) = self.decode(input) {
                    return Some(c);
                }
            }
        }

        /// Read and acknowledge one byte from the shared output buffer without decoding it.
        /// The caller must handle auxiliary (mouse/touchpad) bytes separately.
        pub fn read_raw(&mut self) -> Option<super::RawInput> {
            let status = inb(STATUS);
            if status == 0xff || status & OUTPUT_FULL == 0 {
                return None;
            }
            Some(super::RawInput { code: inb(DATA), auxiliary: status & FROM_AUX != 0 })
        }

        /// Apply the shared scan-code decoder to one controller byte.
        pub fn decode(&mut self, input: super::RawInput) -> Option<u8> {
            if input.auxiliary { None } else { self.0.feed(input.code) }
        }

        /// Read one controller byte and decode it, if it is keyboard input.
        pub fn read_and_decode(&mut self) -> Option<u8> {
            self.read_raw().and_then(|input| self.decode(input))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(codes: &[u8]) -> [Option<u8>; 8] {
        let mut d = Decoder::new();
        let mut out = [None; 8];
        for (o, c) in out.iter_mut().zip(codes) {
            *o = d.feed(*c);
        }
        out
    }

    #[test]
    fn a_key_types_on_press_and_not_on_release() {
        let out = typed(&[0x1e, 0x9e]);
        assert_eq!(out[..2], [Some(b'a'), None]);
    }

    #[test]
    fn shift_applies_while_held() {
        let out = typed(&[0x2a, 0x1e, 0xaa, 0x1e, 0x36, 0x02, 0xb6]);
        assert_eq!(out[1], Some(b'A'));
        assert_eq!(out[3], Some(b'a'));
        assert_eq!(out[5], Some(b'!'));
    }

    #[test]
    fn enter_backspace_and_space() {
        let out = typed(&[0x1c, 0x0e, 0x39]);
        assert_eq!(out[..3], [Some(b'\r'), Some(0x7f), Some(b' ')]);
    }

    #[test]
    fn ctrl_c_is_03() {
        let out = typed(&[0x1d, 0x2e, 0x9d, 0x2e]);
        assert_eq!(out[1], Some(3));
        assert_eq!(out[3], Some(b'c'));
    }

    #[test]
    fn an_extended_key_types_nothing() {
        // The up arrow is e0 48; a plain 0x48 would be keypad 8, which has no entry here.
        assert_eq!(typed(&[0xe0, 0x1c])[..2], [None, None]);
    }

    #[test]
    fn a_fake_shift_does_not_stick() {
        let out = typed(&[0xe0, 0x2a, 0x1e]);
        assert_eq!(out[2], Some(b'a'));
    }

    #[test]
    fn the_right_ctrl_key_works() {
        let out = typed(&[0xe0, 0x1d, 0x2e]);
        assert_eq!(out[2], Some(3));
    }

    #[test]
    fn a_code_past_the_table_is_nothing() {
        assert_eq!(typed(&[0x7f])[0], None);
    }
}
