//! L6 input, from a receiver to the host: keys go on the control channel (`input.key`, reliable and in order, so a
//! key is never left down); the pointer and touch go as small sealed UDP datagrams on the input channel.
//!
//! Datagram: `kind(1)` then, for a pointer (1): `x(2) y(2) buttons(1) wheel(2)`; for touch (2): `count(1)` then each
//! contact `id(1) x(2) y(2) state(1)`. Positions are 0..65535 across the receiver's screen; wheel is in 1/120 notches;
//! touch state 0 down, 1 move, 2 up. While any contact is down the receiver resends the whole set every 100 ms, so a
//! lost "up" is repaired. For a pen (3): `x(2) y(2) pressure(2) flags(1)`, pressure 0..65535 and flags [`pen`] (in
//! range, tip down, barrel button, eraser); while the tip is down the receiver resends it every 100 ms too.

/// Pointer buttons (bit flags).
pub mod buttons {
    pub const LEFT: u8 = 1;
    pub const RIGHT: u8 = 2;
    pub const MIDDLE: u8 = 4;
}

/// Pen flags (bit flags).
pub mod pen {
    /// Over the screen (hovering or touching).
    pub const IN_RANGE: u8 = 1;
    /// The tip is on the screen.
    pub const TIP: u8 = 2;
    /// The side (barrel) button is held.
    pub const BARREL: u8 = 4;
    /// The eraser end is in use.
    pub const ERASER: u8 = 8;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchState {
    Down = 0,
    Move = 1,
    Up = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Contact {
    pub id: u8,
    pub x: u16,
    pub y: u16,
    pub state: TouchState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Pointer { x: u16, y: u16, buttons: u8, wheel: i16 },
    Touch(Vec<Contact>),
    /// A pen (stylus): where, how hard, and its [`pen`] flags.
    Pen { x: u16, y: u16, pressure: u16, flags: u8 },
}

impl Event {
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Event::Pointer { x, y, buttons, wheel } => {
                let mut b = vec![1u8];
                b.extend_from_slice(&x.to_be_bytes());
                b.extend_from_slice(&y.to_be_bytes());
                b.push(*buttons);
                b.extend_from_slice(&wheel.to_be_bytes());
                b
            }
            Event::Touch(contacts) => {
                let mut b = vec![2u8, contacts.len().min(255) as u8];
                for c in contacts.iter().take(255) {
                    b.push(c.id);
                    b.extend_from_slice(&c.x.to_be_bytes());
                    b.extend_from_slice(&c.y.to_be_bytes());
                    b.push(c.state as u8);
                }
                b
            }
            Event::Pen { x, y, pressure, flags } => {
                let mut b = vec![3u8];
                b.extend_from_slice(&x.to_be_bytes());
                b.extend_from_slice(&y.to_be_bytes());
                b.extend_from_slice(&pressure.to_be_bytes());
                b.push(*flags);
                b
            }
        }
    }

    pub fn parse(b: &[u8]) -> Option<Event> {
        let u16_at = |i: usize| b.get(i..i + 2).map(|s| u16::from_be_bytes([s[0], s[1]]));
        match *b.first()? {
            1 if b.len() >= 8 => Some(Event::Pointer { x: u16_at(1)?, y: u16_at(3)?, buttons: b[5], wheel: u16_at(6)? as i16 }),
            2 => {
                let n = *b.get(1)? as usize;
                let mut contacts = Vec::with_capacity(n);
                for k in 0..n {
                    let at = 2 + k * 6;
                    let state = match *b.get(at + 5)? {
                        0 => TouchState::Down,
                        1 => TouchState::Move,
                        2 => TouchState::Up,
                        _ => return None,
                    };
                    contacts.push(Contact { id: b[at], x: u16_at(at + 1)?, y: u16_at(at + 3)?, state });
                }
                Some(Event::Touch(contacts))
            }
            3 if b.len() >= 8 => Some(Event::Pen { x: u16_at(1)?, y: u16_at(3)?, pressure: u16_at(5)?, flags: b[7] }),
            _ => None,
        }
    }
}

/// The additional data every input datagram is sealed with (on the input channel).
pub const AAD: &[u8] = b"odp input";

/// Seals an event as an input datagram.
pub fn seal(event: &Event, sealer: &mut crate::session::Sealer) -> Vec<u8> {
    sealer.seal(AAD, &event.to_bytes())
}

/// Opens an input datagram; None if it doesn't open (another session, changed, replayed) or doesn't parse.
pub fn open(datagram: &[u8], opener: &mut crate::session::Opener) -> Option<Event> {
    Event::parse(&opener.open(AAD, datagram)?)
}

/// Modifier bits of `input.key` (as in a USB HID keyboard report).
pub mod modifiers {
    pub const LEFT_CTRL: u8 = 1;
    pub const LEFT_SHIFT: u8 = 2;
    pub const LEFT_ALT: u8 = 4;
    pub const LEFT_GUI: u8 = 8;
    pub const RIGHT_CTRL: u8 = 16;
    pub const RIGHT_SHIFT: u8 = 32;
    pub const RIGHT_ALT: u8 = 64;
    pub const RIGHT_GUI: u8 = 128;
}

/// Windows virtual-key codes and the HID usages (page 7) they stand for, for implementations on Windows. Generic
/// Shift, Ctrl and Alt (0x10-0x12) are the left ones.
const VK_USAGE: &[(u8, u16)] = &[
    (0x08, 0x2A), (0x09, 0x2B), (0x0D, 0x28), (0x10, 0xE1), (0x11, 0xE0), (0x12, 0xE2), (0x13, 0x48), (0x14, 0x39),
    (0x1B, 0x29), (0x20, 0x2C), (0x21, 0x4B), (0x22, 0x4E), (0x23, 0x4D), (0x24, 0x4A), (0x25, 0x50), (0x26, 0x52),
    (0x27, 0x4F), (0x28, 0x51), (0x2C, 0x46), (0x2D, 0x49), (0x2E, 0x4C), (0x30, 0x27), (0x5B, 0xE3), (0x5C, 0xE7),
    (0x5D, 0x65), (0x60, 0x62), (0x6A, 0x55), (0x6B, 0x57), (0x6D, 0x56), (0x6E, 0x63), (0x6F, 0x54), (0x90, 0x53),
    (0x91, 0x47), (0xA0, 0xE1), (0xA1, 0xE5), (0xA2, 0xE0), (0xA3, 0xE4), (0xA4, 0xE2), (0xA5, 0xE6), (0xBA, 0x33),
    (0xBB, 0x2E), (0xBC, 0x36), (0xBD, 0x2D), (0xBE, 0x37), (0xBF, 0x38), (0xC0, 0x35), (0xDB, 0x2F), (0xDC, 0x31),
    (0xDD, 0x30), (0xDE, 0x34), (0xE2, 0x64),
];

/// The HID usage for a Windows virtual-key code.
pub fn usage_from_vk(vk: u8) -> Option<u16> {
    match vk {
        0x41..=0x5A => Some(0x04 + (vk - 0x41) as u16),
        0x31..=0x39 => Some(0x1E + (vk - 0x31) as u16),
        0x61..=0x69 => Some(0x59 + (vk - 0x61) as u16),
        0x70..=0x7B => Some(0x3A + (vk - 0x70) as u16),
        0x7C..=0x87 => Some(0x68 + (vk - 0x7C) as u16),
        _ => VK_USAGE.iter().find(|(v, _)| *v == vk).map(|(_, u)| *u),
    }
}

/// The Windows virtual-key code for a HID usage (left and right modifiers keep their sides).
pub fn vk_from_usage(usage: u16) -> Option<u8> {
    match usage {
        0x04..=0x1D => Some(0x41 + (usage - 0x04) as u8),
        0x1E..=0x26 => Some(0x31 + (usage - 0x1E) as u8),
        0x59..=0x61 => Some(0x61 + (usage - 0x59) as u8),
        0x3A..=0x45 => Some(0x70 + (usage - 0x3A) as u8),
        0x68..=0x73 => Some(0x7C + (usage - 0x68) as u8),
        // The sided modifiers, not the generic ones listed first.
        0xE0 => Some(0xA2),
        0xE1 => Some(0xA0),
        0xE2 => Some(0xA4),
        _ => VK_USAGE.iter().find(|(_, u)| *u == usage).map(|(v, _)| *v),
    }
}

/// A position on the host's screen of `w` × `h` pixels from a 0..65535 one.
pub fn to_pixels(x: u16, y: u16, w: u32, h: u32) -> (u32, u32) {
    let scale = |v: u16, n: u32| ((v as u64 * n.saturating_sub(1) as u64 + 32767) / 65535) as u32;
    (scale(x, w), scale(y, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_and_touch_round_trip() {
        let p = Event::Pointer { x: 100, y: 65535, buttons: buttons::LEFT | buttons::RIGHT, wheel: -120 };
        assert_eq!(Event::parse(&p.to_bytes()), Some(p));
        let t = Event::Touch(vec![Contact { id: 1, x: 10, y: 20, state: TouchState::Down }, Contact { id: 2, x: 30, y: 40, state: TouchState::Up }]);
        assert_eq!(Event::parse(&t.to_bytes()), Some(t));
        let pen = Event::Pen { x: 32768, y: 1, pressure: 40000, flags: pen::IN_RANGE | pen::TIP | pen::BARREL };
        assert_eq!(pen.to_bytes().len(), 8);
        assert_eq!(Event::parse(&pen.to_bytes()), Some(pen));
        assert_eq!(Event::parse(&[3, 0, 0, 0]), None);
    }

    #[test]
    fn short_or_unknown_datagrams_are_refused() {
        assert_eq!(Event::parse(&[]), None);
        assert_eq!(Event::parse(&[1, 0, 0]), None);
        assert_eq!(Event::parse(&[2, 1, 0, 0, 0, 0, 0]), None, "a contact cut short");
        assert_eq!(Event::parse(&[9]), None);
    }

    #[test]
    fn windows_keys_map_to_hid_usages_and_back() {
        assert_eq!(usage_from_vk(0x41), Some(0x04), "A");
        assert_eq!(usage_from_vk(0x30), Some(0x27), "0");
        assert_eq!(usage_from_vk(0x7B), Some(0x45), "F12");
        assert_eq!(usage_from_vk(0x11), Some(0xE0), "Ctrl is the left one");
        assert_eq!(vk_from_usage(0xE0), Some(0xA2));
        assert_eq!(vk_from_usage(0xE4), Some(0xA3), "right Ctrl");
        for vk in 0u8..=255 {
            if let Some(u) = usage_from_vk(vk) {
                let back = vk_from_usage(u).unwrap();
                let generic = matches!(vk, 0x10..=0x12);
                assert!(back == vk || generic, "{vk:#x} -> {u:#x} -> {back:#x}");
            }
        }
        assert_eq!(usage_from_vk(0x07), None);
    }

    #[test]
    fn input_datagrams_open_only_in_their_session() {
        use crate::keys::Identity;
        use crate::session::{Channel, Handshake, Role};
        let (hi, ri) = (Identity::generate(), Identity::generate());
        let (h, r) = (Handshake::start(Role::Host, &hi), Handshake::start(Role::Receiver, &ri));
        let (ho, ro) = (h.offer(), r.offer());
        let (hs, rs) = (h.sign(&hi, &ro), r.sign(&ri, &ho));
        let (mut host, mut receiver) = (h.finish(&ro, &rs, None).unwrap(), r.finish(&ho, &hs, None).unwrap());
        let e = Event::Pointer { x: 1, y: 2, buttons: buttons::LEFT, wheel: 0 };
        let d = seal(&e, receiver.sealer(Channel::Input));
        assert_eq!(open(&d, host.opener(Channel::Input)), Some(e));
        assert_eq!(open(&d, host.opener(Channel::Input)), None, "replayed");
    }

    #[test]
    fn positions_map_to_the_corners() {
        assert_eq!(to_pixels(0, 0, 1920, 1080), (0, 0));
        assert_eq!(to_pixels(65535, 65535, 1920, 1080), (1919, 1079));
        assert_eq!(to_pixels(32768, 32768, 1920, 1080), (960, 540));
    }
}
