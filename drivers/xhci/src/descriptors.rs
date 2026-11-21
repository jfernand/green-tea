//! Reading a USB configuration descriptor: a run of records, each starting with its length
//! and type. We look for an interface that speaks the HID boot keyboard protocol.

const INTERFACE: u8 = 4;
const ENDPOINT: u8 = 5;
const CLASS_HID: u8 = 3;
const SUBCLASS_BOOT: u8 = 1;
const PROTOCOL_KEYBOARD: u8 = 1;
const ENDPOINT_IN: u8 = 0x80;
const ATTRIBUTES_INTERRUPT: u8 = 3;

/// What it takes to read a boot keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootKeyboard {
    pub configuration: u8,
    pub interface: u8,
    /// The endpoint's number, without the direction bit.
    pub endpoint: u8,
    pub max_packet: u16,
    pub interval: u8,
}

/// The boot keyboard interface in a full configuration descriptor, if it has one.
pub fn find_boot_keyboard(config: &[u8]) -> Option<BootKeyboard> {
    let configuration = *config.get(5)?;
    let mut at = 0;
    let mut interface = None;
    while let (Some(&len), Some(&kind)) = (config.get(at), config.get(at + 1)) {
        if len < 2 {
            return None;
        }
        let d = config.get(at..at + len as usize)?;
        match kind {
            INTERFACE if d.len() >= 9 => {
                let keyboard = d[3] == 0 && d[5] == CLASS_HID && d[6] == SUBCLASS_BOOT && d[7] == PROTOCOL_KEYBOARD;
                interface = keyboard.then_some(d[2]);
            }
            ENDPOINT if d.len() >= 7 => {
                if let Some(interface) = interface
                    && d[2] & ENDPOINT_IN != 0
                    && d[3] & 3 == ATTRIBUTES_INTERRUPT
                {
                    let max_packet = u16::from_le_bytes([d[4], d[5]]) & 0x7ff;
                    return Some(BootKeyboard {
                        configuration,
                        interface,
                        endpoint: d[2] & 0xf,
                        max_packet,
                        interval: d[6],
                    });
                }
            }
            _ => {}
        }
        at += len as usize;
    }
    None
}

/// An endpoint context's Interval: polling every 2^n × 125 µs. High speed and up give the
/// exponent plus one in the descriptor, and full and low speed give a count of milliseconds.
pub fn endpoint_interval(speed: u8, b_interval: u8) -> u8 {
    const FULL: u8 = 1;
    const LOW: u8 = 2;
    if speed == FULL || speed == LOW {
        (b_interval
            .max(1)
            .ilog2() as u8
            + 3)
        .clamp(3, 10)
    } else {
        b_interval.clamp(1, 16) - 1
    }
}

/// The largest control packet at a port speed, until the device says otherwise.
pub fn default_max_packet(speed: u8) -> u16 {
    match speed {
        1 | 2 => 8,
        3 => 64,
        _ => 512,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rustfmt::skip]
    const KEYBOARD: [u8; 34] = [
        9, 2, 34, 0, 1, 7, 0, 0xa0, 50,
        9, 4, 0, 0, 1, 3, 1, 1, 0,
        9, 0x21, 0x11, 1, 0, 1, 0x22, 63, 0,
        7, 5, 0x81, 3, 8, 0, 10,
    ];

    #[test]
    fn finds_a_keyboard() {
        let k = find_boot_keyboard(&KEYBOARD).unwrap();
        assert_eq!(k, BootKeyboard { configuration: 7, interface: 0, endpoint: 1, max_packet: 8, interval: 10 });
    }

    #[test]
    fn a_mouse_is_not_a_keyboard() {
        let mut mouse = KEYBOARD;
        mouse[16] = 2;
        assert_eq!(find_boot_keyboard(&mouse), None);
    }

    #[test]
    fn a_keyboard_may_be_the_second_interface() {
        #[rustfmt::skip]
        let d = [
            9, 2, 41, 0, 2, 1, 0, 0xa0, 50,
            9, 4, 0, 0, 1, 8, 6, 0x50, 0,
            7, 5, 0x82, 2, 0, 2, 0,
            9, 4, 1, 0, 1, 3, 1, 1, 0,
            7, 5, 0x83, 3, 8, 0, 4,
        ];
        let k = find_boot_keyboard(&d).unwrap();
        assert_eq!((k.interface, k.endpoint), (1, 3));
    }

    #[test]
    fn an_out_endpoint_is_skipped() {
        let mut d = KEYBOARD;
        d[29] = 0x01;
        assert_eq!(find_boot_keyboard(&d), None);
    }

    #[test]
    fn a_zero_length_record_ends_the_walk() {
        let mut d = KEYBOARD;
        d[9] = 0;
        assert_eq!(find_boot_keyboard(&d), None);
    }

    #[test]
    fn a_truncated_descriptor_is_refused() {
        assert_eq!(find_boot_keyboard(&KEYBOARD[..30]), None);
    }

    #[test]
    fn intervals_by_speed() {
        assert_eq!(endpoint_interval(1, 10), 6, "full speed: 8 ms is 2^6 × 125 µs");
        assert_eq!(endpoint_interval(2, 1), 3);
        assert_eq!(endpoint_interval(3, 4), 3, "high speed: 2^3 × 125 µs");
        assert_eq!(endpoint_interval(4, 1), 0);
        assert_eq!(endpoint_interval(3, 0), 0);
    }
}
