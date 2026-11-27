//! A stand-in TV for tests (feature `tv`): odp-web's receiver behind a WebSocket client that connects out to a host,
//! as the LG and VIDAA apps do. One receiver per dial, sharing a PIN and the hosts paired so far.

use odp::control::{Caps, Decode};
use odp::keys::Identity;
use odp_web::{Out, Web};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::Message;

fn is_wait(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

pub fn caps() -> Caps {
    Caps { keyboard: true, decode: vec![Decode { codec: "h264".into(), max_w: 1920, max_h: 1080, max_fps: 60, hw: true }], ..Default::default() }
}

/// A TV: its key, the PIN it shows (None: not pairing), the hosts paired, and what reached it.
pub struct Tv {
    pub secret: [u8; 32],
    pub name: String,
    pub pin: Mutex<Option<String>>,
    pub known: Mutex<Vec<(String, [u8; 32])>>,
    pub frames: AtomicU32,
    pub events: Mutex<Vec<serde_json::Value>>,
}

impl Tv {
    pub fn new(secret: [u8; 32], name: &str, pin: Option<&str>) -> Arc<Tv> {
        Arc::new(Tv { secret, name: name.into(), pin: Mutex::new(pin.map(str::to_string)), known: Default::default(), frames: AtomicU32::new(0), events: Default::default() })
    }

    /// Its receiver id (as a hello says it).
    pub fn id(&self) -> String {
        Identity::from_secret(&self.secret).fingerprint().replace('-', "").to_lowercase()
    }

    /// Connects out to the host at `addr` ("127.0.0.1:54334") and answers its dials until `stop`.
    pub fn run(self: Arc<Self>, addr: String, stop: Arc<AtomicBool>) {
        let Ok((mut lobby, _)) = tungstenite::connect(format!("ws://{addr}/odp-out")) else { return };
        let fp = Identity::from_secret(&self.secret).fingerprint();
        let _ = lobby.send(Message::Text(serde_json::json!({"t": "screen", "id": self.id(), "name": self.name, "fp": fp, "pin": "remote"}).to_string()));
        if let tungstenite::stream::MaybeTlsStream::Plain(s) = lobby.get_ref() {
            let _ = s.set_read_timeout(Some(Duration::from_millis(20)));
        }
        while !stop.load(Ordering::SeqCst) {
            match lobby.read() {
                Ok(Message::Text(t)) => {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
                    self.events.lock().unwrap().push(v.clone());
                    if v["t"] == "dial" {
                        let n = v["n"].as_u64().unwrap_or(0);
                        let (me, addr) = (self.clone(), addr.clone());
                        std::thread::spawn(move || me.dial(&addr, n));
                    }
                }
                Ok(_) => {}
                Err(e) if is_wait(&e) => {}
                Err(_) => break,
            }
        }
        let _ = lobby.close(None);
        let _ = lobby.flush();
    }

    /// One connection from the host: a fresh receiver, the shared PIN and paired hosts.
    fn dial(self: Arc<Self>, addr: &str, n: u64) {
        let Ok((mut ws, _)) = tungstenite::connect(format!("ws://{addr}/odp-out/dial/{n}")) else { return };
        let mut web = Web::new(&self.name, caps(), Some(self.secret));
        for (id, key) in self.known.lock().unwrap().iter() {
            web.know(id, *key);
        }
        web.set_pin(self.pin.lock().unwrap().clone());
        web.connected();
        if let tungstenite::stream::MaybeTlsStream::Plain(s) = ws.get_ref() {
            let _ = s.set_read_timeout(Some(Duration::from_millis(2)));
        }
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            match ws.read() {
                Ok(Message::Text(t)) => {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
                    if let Some(p) = v["udp_port"].as_u64() {
                        web.udp_port = p as u16;
                    }
                }
                Ok(Message::Binary(b)) if !b.is_empty() => {
                    let now = (start.elapsed().as_micros()) as i64;
                    if b[0] == 0 {
                        web.control(&b[1..], now);
                    } else {
                        web.media(&b[1..], now);
                    }
                }
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(e) if is_wait(&e) => {}
                Err(_) => break,
            }
            for o in web.take() {
                match o {
                    Out::Control(b) => {
                        let mut m = vec![0u8];
                        m.extend_from_slice(&b);
                        let _ = ws.send(Message::Binary(m));
                    }
                    Out::Media(b) => {
                        let mut m = vec![1u8];
                        m.extend_from_slice(&b);
                        let _ = ws.send(Message::Binary(m));
                    }
                    Out::Frame { .. } => {
                        self.frames.fetch_add(1, Ordering::SeqCst);
                    }
                    Out::Event(e) => {
                        let Ok(v) = serde_json::from_str::<serde_json::Value>(&e) else { continue };
                        if v["t"] == "paired" {
                            if let (Some(id), Some(key)) = (v["id"].as_str(), v["key"].as_str().and_then(odp::link::unb64).and_then(|k| <[u8; 32]>::try_from(k).ok())) {
                                self.known.lock().unwrap().push((id.into(), key));
                            }
                        }
                        self.events.lock().unwrap().push(v);
                    }
                    Out::Audio { .. } => {}
                }
            }
        }
    }
}

