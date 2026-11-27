//! An ODP receiver for browsers (the web profile: LG webOS and Samsung Tizen TVs, any browser). It is "sans I/O":
//! the page feeds it the bytes that arrive and sends the bytes it gives back, so the same pairing, encryption,
//! clock and reassembly run as everywhere else. The page carries them over a WebSocket to the casting computer:
//! control frames (L1, a byte stream) and media datagrams (L4, one per message).
//!
//! [`Web`] is the receiver; `wasm` exports it to JavaScript as plain functions over the module's memory.

pub mod wasm;

use odp::control::{self, Caps, Message, PlayState};
use odp::keys::Identity;
use odp::link::{b64, unb64};
use odp::pair::Pairing;
use odp::session::{Channel, Handshake, Offer, Role, Session};

/// What the page does next.
#[derive(Debug, PartialEq)]
pub enum Out {
    /// Bytes for the control stream.
    Control(Vec<u8>),
    /// One media-channel datagram (a clock ping, the pointer).
    Media(Vec<u8>),
    /// Something to show or keep, as JSON (`t`: pin, paired, started, stopped, cinema.*, problem; and `key` with the
    /// paired host's key to keep).
    Event(String),
    /// An encoded frame (H.264 Annex B), when to show it and when the host captured it (this computer's clock,
    /// microseconds; 0 while the clock is being measured). The page reports its delay from capture to picture.
    Frame { data: Vec<u8>, show_at_us: i64, captured_us: i64, key: bool },
    /// 10 ms of sound (16-bit PCM, 48 kHz stereo) and when to play it (this computer's clock; 0: now).
    Audio { data: Vec<u8>, play_at_us: i64 },
}

enum State {
    /// Waiting for the host's hello.
    Hello,
    /// Hello answered: the host pairs or offers a session.
    PairOrOffer,
    /// Pairing: waiting for the host's pair.confirm.
    Confirm(Box<Pairing>),
    /// Session exchange: our offer is out; waiting for the host's signed offer.
    Signed { hs: Box<Handshake>, theirs: Offer, expected: [u8; 32] },
    Session,
    Closed,
}

pub struct Web {
    identity: Identity,
    id: String,
    name: String,
    caps: Caps,
    pin: Option<String>,
    /// Paired hosts: (id, key).
    known: Vec<(String, [u8; 32])>,
    host: (String, String),
    state: State,
    inbox: Vec<u8>,
    session: Option<Session>,
    media: Option<(odp::session::Sealer, odp::session::Opener)>,
    input: Option<odp::session::Sealer>,
    re: odp::media::Reassembler,
    clock: odp::clock::Clock,
    target: i64,
    last_frame: Option<u32>,
    /// After a gap (or at the start) only a key frame may go to the decoder.
    need_key: bool,
    out: Vec<Out>,
    /// The UDP port the casting computer sends media to (the page's bridge says it).
    pub udp_port: u16,
}

impl Web {
    /// `secret`: this screen's identity key kept from before (32 bytes), or None for a new one (the page keeps it).
    pub fn new(name: &str, caps: Caps, secret: Option<[u8; 32]>) -> Web {
        let identity = secret.map(|s| Identity::from_secret(&s)).unwrap_or_else(Identity::generate);
        let id = identity.fingerprint().replace('-', "").to_lowercase();
        Web {
            identity,
            id,
            name: name.into(),
            caps,
            pin: None,
            known: Vec::new(),
            host: Default::default(),
            state: State::Hello,
            inbox: Vec::new(),
            session: None,
            media: None,
            input: None,
            re: Default::default(),
            clock: Default::default(),
            target: 0,
            last_frame: None,
            need_key: true,
            out: Vec::new(),
            udp_port: 0,
        }
    }

    pub fn secret(&self) -> [u8; 32] {
        self.identity.secret()
    }

    pub fn fingerprint(&self) -> String {
        self.identity.fingerprint()
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// A new connection to the casting computer: the handshake starts again.
    pub fn connected(&mut self) {
        self.state = State::Hello;
        self.inbox.clear();
        self.session = None;
        self.media = None;
        self.input = None;
        self.re = Default::default();
        self.clock = Default::default();
        self.last_frame = None;
        self.need_key = true;
    }

    /// A host paired before (from the page's storage).
    pub fn know(&mut self, id: &str, key: [u8; 32]) {
        self.known.retain(|(i, _)| i != id);
        self.known.push((id.into(), key));
    }

    /// Opens pairing with a new PIN (shown on the page) or closes it.
    pub fn pairing(&mut self, open: bool) {
        self.pin = open.then(odp::pair::new_pin);
        let pin = self.pin.clone();
        self.event(serde_json::json!({"t": "pin", "pin": pin}));
    }

    /// Pairing with a PIN chosen elsewhere (Some opens, None closes), without an event: a screen that takes several
    /// computers at once runs one receiver for each connection and shows one PIN for all of them.
    pub fn set_pin(&mut self, pin: Option<String>) {
        self.pin = pin;
    }

    /// What to do next, in order.
    pub fn take(&mut self) -> Vec<Out> {
        std::mem::take(&mut self.out)
    }

    fn event(&mut self, v: serde_json::Value) {
        self.out.push(Out::Event(v.to_string()));
    }

    fn send(&mut self, m: &Message) {
        let body = match self.session.as_mut() {
            Some(s) => s.sealer(Channel::Control).seal(b"odp control", &m.to_json()),
            None => m.to_json(),
        };
        self.out.push(Out::Control(control::frame(&body)));
    }

    fn fail(&mut self, why: &str) {
        self.state = State::Closed;
        self.event(serde_json::json!({"t": "problem", "text": why}));
    }

    /// Bytes from the control stream.
    pub fn control(&mut self, bytes: &[u8], now_us: i64) {
        self.inbox.extend_from_slice(bytes);
        while self.inbox.len() >= 4 {
            let n = u32::from_be_bytes(self.inbox[..4].try_into().unwrap()) as usize;
            if n == 0 || n > control::MAX_MESSAGE {
                return self.fail("a control message of a wrong size");
            }
            if self.inbox.len() < 4 + n {
                return;
            }
            let body: Vec<u8> = self.inbox.drain(..4 + n).skip(4).collect();
            let plain = match self.session.as_mut() {
                Some(s) => match s.opener(Channel::Control).open(b"odp control", &body) {
                    Some(p) => p,
                    None => return self.fail("a control message that didn't open"),
                },
                None => body,
            };
            match Message::from_json(&plain) {
                Ok(m) => self.message(m, now_us),
                Err(_) => continue, // a type this version doesn't know
            }
        }
    }

    fn message(&mut self, m: Message, now_us: i64) {
        let state = std::mem::replace(&mut self.state, State::Closed);
        self.state = match (state, m) {
            (State::Hello, Message::Hello { id, name, .. }) => {
                self.host = (id, name);
                let caps = self.caps.clone();
                self.send(&Message::Hello { versions: vec![odp::VERSION.into()], id: self.id.clone(), name: self.name.clone(), role: "receiver".into(), caps });
                State::PairOrOffer
            }
            (State::PairOrOffer, Message::PairStart { spake }) => {
                let Some(pin) = self.pin.clone() else {
                    self.send(&Message::Error { id: None, code: "not_pairing".into(), text: "this screen isn't pairing now".into() });
                    return self.fail("not pairing");
                };
                let (mut p, msg) = Pairing::start(Role::Receiver, &pin);
                self.send(&Message::PairStart { spake: b64(&msg) });
                if unb64(&spake).map(|s| p.receive(&s)).is_none_or(|r| r.is_err()) {
                    return self.fail("malformed pairing");
                }
                State::Confirm(Box::new(p))
            }
            (State::Confirm(p), Message::PairConfirm { sealed }) => {
                let Ok(key) = unb64(&sealed).ok_or("malformed").and_then(|s| p.accept(&s)) else {
                    return self.fail("the PIN doesn't match");
                };
                let (id, name) = self.host.clone();
                self.know(&id, key);
                self.pin = None;
                self.event(serde_json::json!({"t": "pin", "pin": null}));
                self.event(serde_json::json!({"t": "paired", "host": name, "id": id, "key": b64(&key)}));
                let confirm = p.confirm(&self.identity).unwrap_or_default();
                self.send(&Message::PairConfirm { sealed: b64(&confirm) });
                State::PairOrOffer
            }
            (State::PairOrOffer, Message::SessionOffer { ephemeral, identity, .. }) => {
                let Some(expected) = self.known.iter().find(|(i, _)| *i == self.host.0).map(|(_, k)| *k) else {
                    return self.fail("an unpaired host that didn't start pairing");
                };
                let (Some(e), Some(i)) = (unb64(&ephemeral).and_then(|v| v.try_into().ok()), unb64(&identity).and_then(|v| v.try_into().ok())) else {
                    return self.fail("a malformed offer");
                };
                let theirs = Offer { ephemeral: e, identity: i };
                let hs = Handshake::start(Role::Receiver, &self.identity);
                let mine = hs.offer();
                let sig = hs.sign(&self.identity, &theirs);
                self.send(&Message::SessionOffer { ephemeral: b64(&mine.ephemeral), identity: b64(&mine.identity), signature: String::new() });
                self.send(&Message::SessionOffer { ephemeral: b64(&mine.ephemeral), identity: b64(&mine.identity), signature: b64(&sig) });
                State::Signed { hs: Box::new(hs), theirs, expected }
            }
            (State::Signed { hs, theirs, expected }, Message::SessionOffer { signature, .. }) => {
                let Some(sig) = unb64(&signature).and_then(|v| <[u8; 64]>::try_from(v).ok()) else {
                    return self.fail("a malformed signature");
                };
                match hs.finish(&theirs, &sig, Some(&expected)) {
                    Ok(s) => {
                        self.media = Some(s.channel_keys(Channel::Media).open(Channel::Media));
                        self.input = Some(s.channel_keys(Channel::Input).open(Channel::Input).0);
                        self.session = Some(s);
                        State::Session
                    }
                    Err(e) => return self.fail(e),
                }
            }
            (State::Session, m) => {
                self.session_message(m, now_us);
                State::Session
            }
            (_, m) => return self.fail(&format!("unexpected {:?}", std::mem::discriminant(&m))),
        };
    }

    fn session_message(&mut self, m: Message, _now_us: i64) {
        let local = |c: &odp::clock::Clock, us: i64| c.to_local(us).unwrap_or(us);
        match m {
            Message::SessionStart { id, w, h, codec, .. } => {
                self.last_frame = None;
                self.need_key = true;
                self.send(&Message::SessionOk { id, udp_port: self.udp_port });
                let host = self.host.1.clone();
                self.event(serde_json::json!({"t": "started", "host": host, "codec": codec, "w": w, "h": h}));
            }
            Message::SessionStop => {
                let host = self.host.1.clone();
                self.event(serde_json::json!({"t": "stopped", "host": host}));
            }
            Message::SyncTarget { delay_us } => self.target = delay_us,
            Message::CinemaOpen { url, position_ms, rate, state, at_us, .. } => {
                let at = local(&self.clock, at_us);
                self.event(serde_json::json!({"t": "cinema.open", "url": url, "position_ms": position_ms, "rate": rate, "playing": state == PlayState::Playing, "at_us": at}));
                // A browser page can't play another site's protected title: the host hears why.
                self.send(&Message::CinemaStatus { position_ms, at_us, state: PlayState::Paused, error: Some("drm_unsupported".into()) });
            }
            Message::CinemaClose => self.event(serde_json::json!({"t": "cinema.close"})),
            Message::Bye => self.state = State::Closed,
            _ => {}
        }
    }

    /// One media datagram (video, a clock pong).
    pub fn media(&mut self, d: &[u8], now_us: i64) {
        let Some((_, opener)) = self.media.as_mut() else { return };
        if d.starts_with(odp::clock::MAGIC) {
            if let Some(odp::clock::Probe::Pong { t1, t2, t3 }) = odp::clock::Probe::open(d, opener) {
                self.clock.add(odp::clock::Sample { t1, t2, t3, t4: now_us });
            }
            return;
        }
        let done = self.re.push(d, opener);
        let missing = self.re.take_missing();
        if !missing.is_empty() {
            self.send(&Message::Nack { stream: 0, seqs: missing });
        }
        let Some(f) = done else { return };
        if f.kind == odp::media::Kind::Audio {
            let play_at_us = self.clock.to_local(f.pts_us).map(|t| t + self.target).unwrap_or(0);
            self.out.push(Out::Audio { data: f.data, play_at_us });
            return;
        }
        // After a gap (or at the start) only a key frame may go to the decoder; ask for one.
        if self.last_frame.is_some_and(|l| f.frame.wrapping_sub(l) != 1) {
            self.need_key = true;
        }
        self.last_frame = Some(f.frame);
        if self.need_key && !f.key {
            self.send(&Message::Keyframe);
            return;
        }
        self.need_key = false;
        let captured_us = self.clock.to_local(f.pts_us).unwrap_or(0);
        let show_at_us = if captured_us != 0 { captured_us + self.target } else { 0 };
        self.out.push(Out::Frame { data: f.data, show_at_us, captured_us, key: f.key });
    }

    /// A clock ping to send now (once a second while a session runs).
    pub fn ping(&mut self, now_us: i64) {
        if let Some((sealer, _)) = self.media.as_mut() {
            let p = odp::clock::Probe::Ping { t1: now_us }.seal(sealer);
            self.out.push(Out::Media(p));
        }
    }

    /// The decoder couldn't use a frame: the next is a key frame.
    pub fn decoder_failed(&mut self) {
        self.need_key = true;
    }

    pub fn key(&mut self, usage: u16, down: bool, modifiers: u8) {
        if self.session.is_some() {
            self.send(&Message::Key { usage, down, modifiers });
        }
    }

    pub fn latency(&mut self, video_us: i64) {
        if self.session.is_some() {
            self.send(&Message::SyncLatency { video_us, audio_us: 0, source: "measured".into() });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    /// A real host (odp::link::host) pairs with the web receiver through a socket the test pumps by hand, then a
    /// second time as a paired host, and a frame comes through.
    #[test]
    fn a_host_pairs_with_the_web_receiver_and_a_frame_arrives() {
        let caps = Caps { keyboard: true, decode: vec![control::Decode { codec: "h264".into(), max_w: 1920, max_h: 1080, max_fps: 60, hw: true }], ..Default::default() };
        let mut web = Web::new("Browser", caps, None);
        web.pairing(true);
        let pin = web.take().into_iter().find_map(|o| match o {
            Out::Event(e) => serde_json::from_str::<serde_json::Value>(&e).ok()?["pin"].as_str().map(str::to_string),
            _ => None,
        });
        let pin = pin.unwrap();
        let mut key = None;
        for round in 0..2 {
            web.connected();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let host_id = Identity::from_secret(&[7u8; 32]);
            let known = key;
            let pin2 = pin.clone();
            let host = std::thread::spawn(move || {
                let s = TcpStream::connect(addr).unwrap();
                let me = odp::link::Me { identity: &host_id, id: "host1", name: "Laptop", caps: Caps::default() };
                let mut link = odp::link::host(s, me, known, if known.is_none() { Some(&pin2) } else { None }).unwrap();
                let peer = link.peer_key;
                if round == 1 {
                    link.send(&Message::SessionStart { id: 1, mode: control::Mode::Mirror, codec: "h264".into(), w: 1280, h: 720, fps: 30, bitrate_kbps: 1000, audio: false, audio_codec: None, group: None, region: None }).unwrap();
                    let ok = link.recv().unwrap();
                    assert!(matches!(ok, Some(Message::SessionOk { .. })), "{ok:?}");
                    let (mut sealer, _) = link.session.channel_keys(Channel::Media).open(Channel::Media);
                    let d = odp::media::Packetizer::new(odp::media::Kind::Video, 0).frame(&[0, 0, 0, 1, 0x65, 1, 2, 3], 1, true, &mut sealer);
                    return (peer, d);
                }
                (peer, Vec::new())
            });
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(std::time::Duration::from_millis(300))).unwrap();
            let mut buf = [0u8; 4096];
            loop {
                for o in web.take() {
                    match o {
                        Out::Control(b) => sock.write_all(&b).unwrap(),
                        Out::Event(e) if e.contains("\"paired\"") => {}
                        _ => {}
                    }
                }
                if host.is_finished() {
                    break;
                }
                match sock.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => web.control(&buf[..n], 0),
                    Err(_) => {}
                }
            }
            let (peer, datagrams) = host.join().unwrap();
            assert_eq!(peer, web.identity.public());
            key = Some(peer);
            if round == 1 {
                for d in &datagrams {
                    web.media(d, 0);
                }
                let media_out = web.take().into_iter().find_map(|o| match o {
                    Out::Frame { data, key, .. } => Some((data, key)),
                    _ => None,
                });
                assert_eq!(media_out, Some((vec![0, 0, 0, 1, 0x65, 1, 2, 3], true)));
            }
        }
    }
}
