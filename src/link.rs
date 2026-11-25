//! L1 + L2 over a byte stream (a TCP connection): `hello` both ways, PIN pairing when the receiver doesn't know the host
//! yet, the signed key exchange, then a [`Link`] that sends and receives sealed control messages.
//!
//! Order on the wire (each a framed JSON message, see [`crate::control`]):
//! 1. host → `hello`, receiver → `hello` (versions, ids, names, the receiver's caps);
//! 2. only when pairing: host → `pair.start`, receiver → `pair.start`, host → `pair.confirm`, receiver → `pair.confirm`;
//! 3. host → `session.offer`, receiver → `session.offer` (ephemeral key, identity key, signature);
//! 4. sealed from here on.

use std::io::{Read, Write};

use crate::control::{self, Caps, Message};
use crate::keys::Identity;
use crate::pair::Pairing;
use crate::session::{Channel, Handshake, Offer, Role, Session};

/// An encrypted control link.
pub struct Link<S: Read + Write> {
    stream: S,
    pub session: Session,
    /// The other side's `hello`.
    pub peer_id: String,
    pub peer_name: String,
    pub peer_caps: Caps,
    /// The other side's identity key (to store after pairing).
    pub peer_key: [u8; 32],
}

fn b64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for ch in s.bytes().filter(|&b| b != b'=') {
        let v = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn key32(s: &str) -> Result<[u8; 32], String> {
    unb64(s).and_then(|v| v.try_into().ok()).ok_or_else(|| "a malformed key".to_string())
}

fn send_plain(s: &mut impl Write, m: &Message) -> Result<(), String> {
    s.write_all(&control::frame(&m.to_json())).and_then(|_| s.flush()).map_err(|e| e.to_string())
}

fn recv_plain(s: &mut impl Read) -> Result<Message, String> {
    let body = control::read_frame(s).map_err(|e| e.to_string())?.ok_or("the other side closed the connection")?;
    let m = Message::from_json(&body)?;
    if let Message::Error { text, .. } = &m {
        return Err(text.clone());
    }
    Ok(m)
}

/// Who this side is, for `hello`.
pub struct Me<'a> {
    pub identity: &'a Identity,
    pub id: &'a str,
    pub name: &'a str,
    pub caps: Caps,
}

/// The host's side. `known` is the receiver's identity key stored at pairing (None: pair now with `pin`).
pub fn host<S: Read + Write>(mut stream: S, me: Me<'_>, known: Option<[u8; 32]>, pin: Option<&str>) -> Result<Link<S>, String> {
    send_plain(&mut stream, &Message::Hello { versions: vec![crate::VERSION.into()], id: me.id.into(), name: me.name.into(), role: "host".into(), caps: Caps::default() })?;
    let Message::Hello { id, name, caps, .. } = recv_plain(&mut stream)? else { return Err("expected hello".into()) };
    let expected = match known {
        Some(k) => k,
        None => {
            let pin = pin.ok_or("this receiver isn't paired: enter the PIN it shows")?;
            let (mut p, msg) = Pairing::start(Role::Host, pin);
            send_plain(&mut stream, &Message::PairStart { spake: b64(&msg) })?;
            let Message::PairStart { spake } = recv_plain(&mut stream)? else { return Err("expected pair.start".into()) };
            p.receive(&unb64(&spake).ok_or("malformed pairing")?)?;
            send_plain(&mut stream, &Message::PairConfirm { sealed: b64(&p.confirm(me.identity)?) })?;
            let Message::PairConfirm { sealed } = recv_plain(&mut stream)? else { return Err("expected pair.confirm".into()) };
            p.accept(&unb64(&sealed).ok_or("malformed pairing")?)?
        }
    };
    let link = exchange(&mut stream, Role::Host, me.identity, Some(expected))?;
    Ok(Link { stream, session: link, peer_id: id, peer_name: name, peer_caps: caps, peer_key: expected })
}

/// What a receiver knows when a host connects.
pub struct ReceiverPolicy<'a> {
    /// The identity key stored for this host id, if it was paired before.
    pub paired: &'a dyn Fn(&str) -> Option<[u8; 32]>,
    /// The PIN shown now, if pairing is open (None: refuse unknown hosts).
    pub pin: Option<String>,
}

/// The receiver's side.
pub fn receiver<S: Read + Write>(mut stream: S, me: Me<'_>, policy: ReceiverPolicy<'_>) -> Result<Link<S>, String> {
    let Message::Hello { id, name, .. } = recv_plain(&mut stream)? else { return Err("expected hello".into()) };
    send_plain(&mut stream, &Message::Hello { versions: vec![crate::VERSION.into()], id: me.id.into(), name: me.name.into(), role: "receiver".into(), caps: me.caps })?;
    let expected = match (policy.paired)(&id) {
        Some(k) => Some(k),
        None => {
            // The host starts pairing; without a PIN on screen there's nothing to pair with.
            let first = recv_plain(&mut stream)?;
            let Message::PairStart { spake } = first else { return Err("an unpaired host that didn't start pairing".into()) };
            let Some(pin) = policy.pin else {
                let _ = send_plain(&mut stream, &Message::Error { id: None, code: "not_pairing".into(), text: "this screen isn't pairing now".into() });
                return Err("not pairing".into());
            };
            let (mut p, msg) = Pairing::start(Role::Receiver, &pin);
            send_plain(&mut stream, &Message::PairStart { spake: b64(&msg) })?;
            p.receive(&unb64(&spake).ok_or("malformed pairing")?)?;
            let Message::PairConfirm { sealed } = recv_plain(&mut stream)? else { return Err("expected pair.confirm".into()) };
            let host_key = p.accept(&unb64(&sealed).ok_or("malformed pairing")?)?;
            send_plain(&mut stream, &Message::PairConfirm { sealed: b64(&p.confirm(me.identity)?) })?;
            Some(host_key)
        }
    };
    let session = exchange(&mut stream, Role::Receiver, me.identity, expected)?;
    let peer_key = session.peer;
    Ok(Link { stream, session, peer_id: id, peer_name: name, peer_caps: Caps::default(), peer_key })
}

fn exchange<S: Read + Write>(stream: &mut S, role: Role, me: &Identity, expected: Option<[u8; 32]>) -> Result<Session, String> {
    let hs = Handshake::start(role, me);
    let mine = hs.offer();
    let send_offer = |stream: &mut S, sig: Option<[u8; 64]>| {
        send_plain(stream, &Message::SessionOffer { ephemeral: b64(&mine.ephemeral), identity: b64(&mine.identity), signature: sig.map(|s| b64(&s)).unwrap_or_default() })
    };
    // Offers first (no signature yet: each signature covers both offers), then the signatures.
    send_offer(stream, None)?;
    let Message::SessionOffer { ephemeral, identity, .. } = recv_plain(stream)? else { return Err("expected session.offer".into()) };
    let theirs = Offer { ephemeral: key32(&ephemeral)?, identity: key32(&identity)? };
    send_offer(stream, Some(hs.sign(me, &theirs)))?;
    let Message::SessionOffer { signature, .. } = recv_plain(stream)? else { return Err("expected the signed session.offer".into()) };
    let sig: [u8; 64] = unb64(&signature).and_then(|v| v.try_into().ok()).ok_or("a malformed signature")?;
    hs.finish(&theirs, &sig, expected.as_ref()).map_err(str::to_string)
}

impl<S: Read + Write> Link<S> {
    /// Sends a message sealed on the control channel.
    pub fn send(&mut self, m: &Message) -> Result<(), String> {
        let sealed = self.session.sealer(Channel::Control).seal(b"odp control", &m.to_json());
        self.stream.write_all(&control::frame(&sealed)).and_then(|_| self.stream.flush()).map_err(|e| e.to_string())
    }

    /// Receives the next sealed message; Ok(None) when the other side closed.
    pub fn recv(&mut self) -> Result<Option<Message>, String> {
        let Some(body) = control::read_frame(&mut self.stream).map_err(|e| e.to_string())? else { return Ok(None) };
        let plain = self.session.opener(Channel::Control).open(b"odp control", &body).ok_or("a control message that didn't open")?;
        Message::from_json(&plain).map(Some)
    }

    pub fn stream(&self) -> &S {
        &self.stream
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    fn me<'a>(identity: &'a Identity, id: &'a str) -> Me<'a> {
        Me { identity, id, name: id, caps: Caps { keyboard: true, ..Default::default() } }
    }

    #[test]
    fn b64_round_trips() {
        for n in 0..40 {
            let v: Vec<u8> = (0..n).map(|i| (i * 37 % 256) as u8).collect();
            assert_eq!(unb64(&b64(&v)).unwrap(), v);
        }
        assert_eq!(b64(b"Man"), "TWFu");
    }

    /// Pairs with a PIN, then connects again as a paired host, exchanging a sealed message each time.
    #[test]
    fn pair_with_a_pin_then_reconnect_paired() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (host_id, recv_id) = (Identity::generate(), Identity::generate());
        let recv_pub = recv_id.public();
        let host_pub = host_id.public();
        let receiver_side = std::thread::spawn(move || {
            let mut stored: Option<[u8; 32]> = None;
            for round in 0..2 {
                let (s, _) = listener.accept().unwrap();
                let known = stored;
                let paired = move |_: &str| known;
                let pin = if round == 0 { Some("123456".to_string()) } else { None };
                let mut link = receiver(s, me(&recv_id, "screen"), ReceiverPolicy { paired: &paired, pin }).unwrap();
                stored = Some(link.peer_key);
                assert_eq!(link.recv().unwrap(), Some(Message::SyncOffset { us: round }));
                link.send(&Message::Keyframe).unwrap();
            }
            stored.unwrap()
        });
        let mut link = host(TcpStream::connect(addr).unwrap(), me(&host_id, "laptop"), None, Some("123456")).unwrap();
        assert_eq!(link.peer_key, recv_pub);
        assert!(link.peer_caps.keyboard);
        link.send(&Message::SyncOffset { us: 0 }).unwrap();
        assert_eq!(link.recv().unwrap(), Some(Message::Keyframe));
        drop(link);
        let mut again = host(TcpStream::connect(addr).unwrap(), me(&host_id, "laptop"), Some(recv_pub), None).unwrap();
        again.send(&Message::SyncOffset { us: 1 }).unwrap();
        assert_eq!(again.recv().unwrap(), Some(Message::Keyframe));
        assert_eq!(receiver_side.join().unwrap(), host_pub);
    }

    #[test]
    fn a_wrong_pin_or_a_screen_not_pairing_refuses() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let recv_id = Identity::generate();
        let receiver_side = std::thread::spawn(move || {
            let none = |_: &str| None;
            let (s, _) = listener.accept().unwrap();
            let wrong = receiver(s, me(&recv_id, "screen"), ReceiverPolicy { paired: &none, pin: Some("111111".into()) }).err();
            let (s, _) = listener.accept().unwrap();
            let closed = receiver(s, me(&recv_id, "screen"), ReceiverPolicy { paired: &none, pin: None }).err();
            (wrong, closed)
        });
        let host_id = Identity::generate();
        assert!(host(TcpStream::connect(addr).unwrap(), me(&host_id, "laptop"), None, Some("222222")).is_err());
        let refused = host(TcpStream::connect(addr).unwrap(), me(&host_id, "laptop"), None, Some("222222")).err().unwrap();
        assert_eq!(refused, "this screen isn't pairing now");
        let (wrong, closed) = receiver_side.join().unwrap();
        assert_eq!(wrong.as_deref(), Some("the PIN doesn't match"));
        assert_eq!(closed.as_deref(), Some("not pairing"));
    }
}
