//! L2 pairing with a PIN: the receiver shows a 6-digit PIN; both sides run SPAKE2 (Ed25519 group) with it, then each
//! sends its identity key sealed under a key only a side that knew the PIN can have. A wrong PIN fails the seal check
//! and tells an attacker nothing about the PIN beyond that one guess, so receivers allow few tries (see [`Guard`]).

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};

use crate::keys::Identity;
use crate::session::Role;

/// One side of a pairing in progress.
pub struct Pairing {
    role: Role,
    state: Option<Spake2<Ed25519Group>>,
    keys: Option<([u8; 32], [u8; 32])>,
}

impl Pairing {
    /// Starts with the PIN; returns the message to send the other side.
    pub fn start(role: Role, pin: &str) -> (Pairing, Vec<u8>) {
        let (a, b) = (SpakeId::new(b"odp host"), SpakeId::new(b"odp receiver"));
        let pw = Password::new(pin.as_bytes());
        let (state, msg) = match role {
            Role::Host => Spake2::<Ed25519Group>::start_a(&pw, &a, &b),
            Role::Receiver => Spake2::<Ed25519Group>::start_b(&pw, &a, &b),
        };
        (Pairing { role, state: Some(state), keys: None }, msg)
    }

    /// Takes the other side's message; then [`Pairing::confirm`] and [`Pairing::accept`].
    pub fn receive(&mut self, theirs: &[u8]) -> Result<(), &'static str> {
        let state = self.state.take().ok_or("used twice")?;
        let shared = state.finish(theirs).map_err(|_| "a malformed pairing message")?;
        let hk = Hkdf::<Sha256>::new(Some(b"odp/0.1 pair"), &shared);
        let mut h2r = [0u8; 32];
        let mut r2h = [0u8; 32];
        hk.expand(b"h2r", &mut h2r).expect("valid length");
        hk.expand(b"r2h", &mut r2h).expect("valid length");
        self.keys = Some(match self.role {
            Role::Host => (h2r, r2h),
            Role::Receiver => (r2h, h2r),
        });
        Ok(())
    }

    /// This side's identity key, sealed for the other side.
    pub fn confirm(&self, me: &Identity) -> Result<Vec<u8>, &'static str> {
        let (send, _) = self.keys.ok_or("no pairing message yet")?;
        ChaCha20Poly1305::new(&send.into()).encrypt(Nonce::from_slice(&[0u8; 12]), me.public().as_slice()).map_err(|_| "sealing failed")
    }

    /// The other side's identity key from its confirmation. Err when the PIN differed (or the message was changed).
    pub fn accept(&self, theirs: &[u8]) -> Result<[u8; 32], &'static str> {
        let (_, recv) = self.keys.ok_or("no pairing message yet")?;
        let key = ChaCha20Poly1305::new(&recv.into()).decrypt(Nonce::from_slice(&[0u8; 12]), theirs).map_err(|_| "the PIN doesn't match")?;
        key.try_into().map_err(|_| "a malformed identity key")
    }
}

/// A new random 6-digit PIN for a receiver to show.
pub fn new_pin() -> String {
    use rand_core::{OsRng, RngCore};
    format!("{:06}", OsRng.next_u32() % 1_000_000)
}

/// A receiver's limit on wrong PINs: after 3 failures it refuses pairing for 30 seconds.
#[derive(Default)]
pub struct Guard {
    failures: u32,
    locked_until: Option<std::time::Instant>,
}

impl Guard {
    pub fn may_try(&mut self) -> bool {
        match self.locked_until {
            Some(t) if std::time::Instant::now() < t => false,
            Some(_) => {
                self.locked_until = None;
                self.failures = 0;
                true
            }
            None => true,
        }
    }

    pub fn failed(&mut self) {
        self.failures += 1;
        if self.failures >= 3 {
            self.locked_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(30));
        }
    }

    pub fn succeeded(&mut self) {
        self.failures = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(host_pin: &str, receiver_pin: &str) -> (Result<[u8; 32], &'static str>, Result<[u8; 32], &'static str>, Identity, Identity) {
        let (hi, ri) = (Identity::generate(), Identity::generate());
        let (mut h, hm) = Pairing::start(Role::Host, host_pin);
        let (mut r, rm) = Pairing::start(Role::Receiver, receiver_pin);
        h.receive(&rm).unwrap();
        r.receive(&hm).unwrap();
        let (hc, rc) = (h.confirm(&hi).unwrap(), r.confirm(&ri).unwrap());
        (h.accept(&rc), r.accept(&hc), hi, ri)
    }

    #[test]
    fn the_same_pin_exchanges_identities() {
        let (host_sees, receiver_sees, hi, ri) = run("482913", "482913");
        assert_eq!(host_sees.unwrap(), ri.public());
        assert_eq!(receiver_sees.unwrap(), hi.public());
    }

    #[test]
    fn a_different_pin_fails_on_both_sides() {
        let (host_sees, receiver_sees, _, _) = run("482913", "482914");
        assert_eq!(host_sees, Err("the PIN doesn't match"));
        assert_eq!(receiver_sees, Err("the PIN doesn't match"));
    }

    #[test]
    fn pins_are_six_digits() {
        for _ in 0..50 {
            let p = new_pin();
            assert!(p.len() == 6 && p.chars().all(|c| c.is_ascii_digit()), "{p}");
        }
    }

    #[test]
    fn three_wrong_pins_lock_pairing() {
        let mut g = Guard::default();
        for _ in 0..3 {
            assert!(g.may_try());
            g.failed();
        }
        assert!(!g.may_try());
    }
}
