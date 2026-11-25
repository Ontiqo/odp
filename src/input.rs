//! L6 input, from a receiver to the host: keys go on the control channel (`input.key`, reliable and in order, so a
//! key is never left down); the pointer and touch go as small sealed UDP datagrams on the input channel.
//!
//! Datagram: `kind(1)` then, for a pointer (1): `x(2) y(2) buttons(1) wheel(2)`; for touch (2): `count(1)` then each
//! contact `id(1) x(2) y(2) state(1)`. Positions are 0..65535 across the receiver's screen; wheel is in 1/120 notches;
//! touch state 0 down, 1 move, 2 up. While any contact is down the receiver resends the whole set every 100 ms, so a
//! lost "up" is repaired.

/// Pointer buttons (bit flags).
pub mod buttons {
    pub const LEFT: u8 = 1;
    pub const RIGHT: u8 = 2;
    pub const MIDDLE: u8 = 4;
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
            _ => None,
        }
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
    }

    #[test]
    fn short_or_unknown_datagrams_are_refused() {
        assert_eq!(Event::parse(&[]), None);
        assert_eq!(Event::parse(&[1, 0, 0]), None);
        assert_eq!(Event::parse(&[2, 1, 0, 0, 0, 0, 0]), None, "a contact cut short");
        assert_eq!(Event::parse(&[9]), None);
    }

    #[test]
    fn positions_map_to_the_corners() {
        assert_eq!(to_pixels(0, 0, 1920, 1080), (0, 0));
        assert_eq!(to_pixels(65535, 65535, 1920, 1080), (1919, 1079));
        assert_eq!(to_pixels(32768, 32768, 1920, 1080), (960, 540));
    }
}
