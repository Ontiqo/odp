//! L2 sessions: an ephemeral X25519 exchange, each side signing the exchange with its identity key, then HKDF-SHA-256
//! gives separate keys for control, media and input in each direction. Everything after is sealed with
//! ChaCha20-Poly1305; the 12-byte nonce is a 4-byte channel tag and an 8-byte counter that never repeats under one key.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use hkdf::Hkdf;
use rand_core::OsRng;
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey};

use crate::keys::{self, Identity};

/// Which side of the session this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Host,
    Receiver,
}

/// What a key is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Channel {
    Control,
    Media,
    Input,
}

impl Channel {
    fn label(self) -> &'static str {
        match self {
            Channel::Control => "control",
            Channel::Media => "media",
            Channel::Input => "input",
        }
    }
}

/// The first message of the exchange from each side: its ephemeral public key, its identity key and its signature
/// over both ephemeral keys and both identities (so neither can be swapped).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub ephemeral: [u8; 32],
    pub identity: [u8; 32],
}

/// One side of a session being set up.
pub struct Handshake {
    role: Role,
    secret: Option<EphemeralSecret>,
    offer: Offer,
}

/// The text both sides sign: the protocol, then the host's and the receiver's ephemeral and identity keys.
fn transcript(host: &Offer, receiver: &Offer) -> Vec<u8> {
    let mut t = b"odp/0.1 session".to_vec();
    for k in [&host.ephemeral, &host.identity, &receiver.ephemeral, &receiver.identity] {
        t.extend_from_slice(k);
    }
    t
}

impl Handshake {
    pub fn start(role: Role, me: &Identity) -> Handshake {
        let secret = EphemeralSecret::random_from_rng(OsRng);
        let ephemeral = PublicKey::from(&secret).to_bytes();
        Handshake { role, secret: Some(secret), offer: Offer { ephemeral, identity: me.public() } }
    }

    /// What to send the other side first.
    pub fn offer(&self) -> Offer {
        self.offer.clone()
    }

    /// This side's signature over the exchange, once the other side's offer is known.
    pub fn sign(&self, me: &Identity, theirs: &Offer) -> [u8; 64] {
        let (h, r) = self.order(theirs);
        me.sign(&transcript(h, r))
    }

    fn order<'a>(&'a self, theirs: &'a Offer) -> (&'a Offer, &'a Offer) {
        match self.role {
            Role::Host => (&self.offer, theirs),
            Role::Receiver => (theirs, &self.offer),
        }
    }

    /// Checks the other side's signature and makes the session keys. `expected` pins the other side's identity (the
    /// key stored at pairing); None accepts any (only while pairing, where the PAKE vouches for it).
    pub fn finish(mut self, theirs: &Offer, their_signature: &[u8; 64], expected: Option<&[u8; 32]>) -> Result<Session, &'static str> {
        if expected.is_some_and(|k| *k != theirs.identity) {
            return Err("the other side isn't the device paired with");
        }
        let t = {
            let (h, r) = self.order(theirs);
            transcript(h, r)
        };
        if !keys::verify(&theirs.identity, &t, their_signature) {
            return Err("the other side's signature doesn't verify");
        }
        let secret = self.secret.take().ok_or("used twice")?;
        let shared = secret.diffie_hellman(&PublicKey::from(theirs.ephemeral));
        if !shared.was_contributory() {
            return Err("a weak key from the other side");
        }
        let hk = Hkdf::<Sha256>::new(Some(&t), shared.as_bytes());
        let key = |dir: &str, ch: Channel| {
            let mut k = [0u8; 32];
            hk.expand(format!("odp/0.1 {dir} {}", ch.label()).as_bytes(), &mut k).expect("32 bytes is a valid HKDF length");
            k
        };
        let (send_dir, recv_dir) = match self.role {
            Role::Host => ("h2r", "r2h"),
            Role::Receiver => ("r2h", "h2r"),
        };
        let send_keys = [Channel::Control, Channel::Media, Channel::Input].map(|c| key(send_dir, c));
        let recv_keys = [Channel::Control, Channel::Media, Channel::Input].map(|c| key(recv_dir, c));
        Ok(Session {
            peer: theirs.identity,
            send: [Channel::Control, Channel::Media, Channel::Input].map(|c| Sealer::new(key(send_dir, c), c)),
            recv: [Channel::Control, Channel::Media, Channel::Input].map(|c| Opener::new(key(recv_dir, c), c)),
            keys: (send_keys, recv_keys),
        })
    }
}

/// An established session: a sealer and an opener per channel.
pub struct Session {
    pub peer: [u8; 32],
    send: [Sealer; 3],
    recv: [Opener; 3],
    keys: ([[u8; 32]; 3], [[u8; 32]; 3]),
}

/// One channel's keys in both directions, to hand to another process of the same device (a media helper) over a
/// private pipe, so it can seal and open that channel without a handshake of its own. The two processes must not both
/// seal with them: the counters would repeat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelKeys {
    pub channel_send: [u8; 32],
    pub channel_recv: [u8; 32],
}

impl ChannelKeys {
    /// A sealer and an opener for `c` from handed-over keys (in the helper process).
    pub fn open(&self, c: Channel) -> (Sealer, Opener) {
        (Sealer::new(self.channel_send, c), Opener::new(self.channel_recv, c))
    }

    /// As 128 hex digits (send, then receive), for the pipe.
    pub fn to_hex(&self) -> String {
        self.channel_send.iter().chain(self.channel_recv.iter()).map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(s: &str) -> Option<ChannelKeys> {
        let s = s.trim();
        if s.len() != 128 {
            return None;
        }
        let bytes: Option<Vec<u8>> = (0..128).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect();
        let bytes = bytes?;
        Some(ChannelKeys { channel_send: bytes[..32].try_into().ok()?, channel_recv: bytes[32..].try_into().ok()? })
    }
}

fn index(c: Channel) -> usize {
    match c {
        Channel::Control => 0,
        Channel::Media => 1,
        Channel::Input => 2,
    }
}

impl Session {
    pub fn sealer(&mut self, c: Channel) -> &mut Sealer {
        &mut self.send[index(c)]
    }
    pub fn opener(&mut self, c: Channel) -> &mut Opener {
        &mut self.recv[index(c)]
    }

    /// One channel's keys, to hand to a helper process of this device (see [`ChannelKeys`]).
    pub fn channel_keys(&self, c: Channel) -> ChannelKeys {
        ChannelKeys { channel_send: self.keys.0[index(c)], channel_recv: self.keys.1[index(c)] }
    }
}

fn nonce(channel: Channel, counter: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..4].copy_from_slice(match channel {
        Channel::Control => b"odpc",
        Channel::Media => b"odpm",
        Channel::Input => b"odpi",
    });
    n[4..].copy_from_slice(&counter.to_be_bytes());
    n
}

/// Seals messages in one direction of one channel. Output: 8-byte counter, then ciphertext and tag.
pub struct Sealer {
    cipher: ChaCha20Poly1305,
    channel: Channel,
    next: u64,
}

impl Sealer {
    fn new(key: [u8; 32], channel: Channel) -> Sealer {
        Sealer { cipher: ChaCha20Poly1305::new(&key.into()), channel, next: 0 }
    }

    /// Seals `plaintext`; `aad` (a packet header, say) is authenticated but sent as it is.
    pub fn seal(&mut self, aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
        let counter = self.next;
        self.next += 1;
        let n = nonce(self.channel, counter);
        let sealed = self.cipher.encrypt(Nonce::from_slice(&n), Payload { msg: plaintext, aad }).expect("ChaCha20-Poly1305 never fails to encrypt");
        let mut out = counter.to_be_bytes().to_vec();
        out.extend_from_slice(&sealed);
        out
    }
}

/// Opens messages in one direction of one channel: a counter is accepted once, and only within [`WINDOW`] of the
/// highest seen (packets may arrive out of order; a recording played back doesn't open).
pub struct Opener {
    cipher: ChaCha20Poly1305,
    channel: Channel,
    highest: Option<u64>,
    seen: std::collections::HashSet<u64>,
}

pub const WINDOW: u64 = 1024;

impl Opener {
    fn new(key: [u8; 32], channel: Channel) -> Opener {
        Opener { cipher: ChaCha20Poly1305::new(&key.into()), channel, highest: None, seen: Default::default() }
    }

    /// Opens a sealed message (as [`Sealer::seal`] made it) with the same `aad`. None if it was changed, sealed under
    /// another key, already seen, or too old.
    pub fn open(&mut self, aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
        if sealed.len() < 8 + 16 {
            return None;
        }
        let counter = u64::from_be_bytes(sealed[..8].try_into().unwrap());
        let fresh = match self.highest {
            Some(h) if counter <= h => h - counter < WINDOW && !self.seen.contains(&counter),
            _ => true,
        };
        if !fresh {
            return None;
        }
        let plain = self.cipher.decrypt(Nonce::from_slice(&nonce(self.channel, counter)), Payload { msg: &sealed[8..], aad }).ok()?;
        self.seen.insert(counter);
        let h = self.highest.map_or(counter, |h| h.max(counter));
        self.highest = Some(h);
        if self.seen.len() as u64 > 2 * WINDOW {
            self.seen.retain(|&s| h - s < WINDOW);
        }
        Some(plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Session, Session, Identity, Identity) {
        let (hi, ri) = (Identity::generate(), Identity::generate());
        let (h, r) = (Handshake::start(Role::Host, &hi), Handshake::start(Role::Receiver, &ri));
        let (ho, ro) = (h.offer(), r.offer());
        let (hs, rs) = (h.sign(&hi, &ro), r.sign(&ri, &ho));
        let host = h.finish(&ro, &rs, Some(&ri.public())).unwrap();
        let receiver = r.finish(&ho, &hs, Some(&hi.public())).unwrap();
        (host, receiver, hi, ri)
    }

    #[test]
    fn both_sides_get_matching_keys_for_each_direction_and_channel() {
        let (mut host, mut receiver, _, _) = pair();
        let s = host.sealer(Channel::Media).seal(b"hdr", b"frame");
        assert_eq!(receiver.opener(Channel::Media).open(b"hdr", &s).unwrap(), b"frame");
        let back = receiver.sealer(Channel::Input).seal(b"", b"key");
        assert_eq!(host.opener(Channel::Input).open(b"", &back).unwrap(), b"key");
        // Another channel's key, a changed header, or the same direction's own key don't open it.
        assert!(receiver.opener(Channel::Control).open(b"hdr", &s).is_none());
        let s2 = host.sealer(Channel::Media).seal(b"hdr", b"frame");
        assert!(receiver.opener(Channel::Media).open(b"HDR", &s2).is_none());
        let own = host.sealer(Channel::Media).seal(b"hdr", b"x");
        assert!(host.opener(Channel::Media).open(b"hdr", &own).is_none(), "a side can't open its own direction");
    }

    #[test]
    fn a_pinned_identity_is_enforced_and_signatures_are_checked() {
        let (hi, ri, other) = (Identity::generate(), Identity::generate(), Identity::generate());
        let (h, r) = (Handshake::start(Role::Host, &hi), Handshake::start(Role::Receiver, &ri));
        let ro = r.offer();
        let rs = r.sign(&ri, &h.offer());
        assert_eq!(Handshake::start(Role::Host, &hi).finish(&ro, &rs, Some(&other.public())).err(), Some("the other side isn't the device paired with"));
        let forged = other.sign(b"anything");
        assert_eq!(h.finish(&ro, &forged, None).err(), Some("the other side's signature doesn't verify"));
    }

    #[test]
    fn a_helper_process_can_use_a_channel_from_handed_over_keys() {
        let (host, mut receiver, _, _) = pair();
        let keys = ChannelKeys::from_hex(&host.channel_keys(Channel::Media).to_hex()).unwrap();
        let (mut sealer, _) = keys.open(Channel::Media);
        let s = sealer.seal(b"h", b"frame from the helper");
        assert_eq!(receiver.opener(Channel::Media).open(b"h", &s).unwrap(), b"frame from the helper");
        assert!(ChannelKeys::from_hex("zz").is_none());
    }

    #[test]
    fn replays_are_refused_and_reordering_within_the_window_is_fine() {
        let (mut host, mut receiver, _, _) = pair();
        let msgs: Vec<Vec<u8>> = (0..300).map(|i| host.sealer(Channel::Media).seal(b"", &[i as u8])).collect();
        let o = receiver.opener(Channel::Media);
        assert!(o.open(b"", &msgs[299]).is_some());
        assert!(o.open(b"", &msgs[10]).is_some(), "late but inside the window");
        assert!(o.open(b"", &msgs[10]).is_none(), "a replay");
        assert!(o.open(b"", &msgs[299]).is_none(), "a replay of the newest");
        assert!(o.open(b"", &msgs[200]).is_some());
        assert!(o.open(b"", &msgs[200]).is_none());
        let far: Vec<Vec<u8>> = (0..1200).map(|_| host.sealer(Channel::Media).seal(b"", b"x")).collect();
        assert!(o.open(b"", &far[1199]).is_some());
        assert!(o.open(b"", &msgs[0]).is_none(), "outside the window");
        assert!(o.open(b"", &far[300]).is_some(), "inside the window, never seen");
    }
}
