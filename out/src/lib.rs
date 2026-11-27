//! Connect-out (SPEC.md, "Connect-out"): screens whose apps can only make outgoing connections (TV web apps on LG
//! webOS and Hisense VIDAA) dial the host over WebSockets, and this bridge makes each one an ordinary ODP screen on a
//! loopback address, so a host's pairing, casting and checks work unchanged.
//!
//! - The screen keeps a **lobby** WebSocket open at `/odp-out` and says who it is first:
//!   `{"t":"screen","id","name","fp","pin"}` (it may say it again after a rename). The bridge answers
//!   `{"t":"host","name"}`.
//! - For the host the screen is at `127.0.0.1:<port>` ([`Screen::address`]). Each TCP connection the host makes there
//!   becomes a **dial**: the bridge asks the screen over the lobby (`{"t":"dial","n"}`), the screen opens
//!   `/odp-out/dial/<n>`, and that WebSocket carries the web profile for that one connection: binary messages of one
//!   channel byte (0: L1 control bytes, 1: one L4 datagram) and the bytes, after a first text `{"udp_port"}` from the
//!   bridge (the loopback UDP port to give in `session.ok`, where the host's media then goes).
//! - L2 is end to end: the bridge relays bytes it can't read.
//!
//! Each WebSocket is owned by one thread that writes what the others queue for it and reads in between.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::handshake::server::{Request, Response};
use tungstenite::{Message, WebSocket};

/// The port hosts listen on for connect-out screens (any free one when it is taken; announced as `_odp-host._tcp`).
pub const PORT: u16 = 54334;
/// A lobby that says nothing (not even a pong) for this long is gone.
const QUIET: Duration = Duration::from_secs(30);

/// A screen connected out to this host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screen {
    /// Its receiver id, name and key fingerprint, as its `hello` and announcement would say.
    pub id: String,
    pub name: String,
    pub fingerprint: String,
    /// How a PIN is brought up there (`remote`, `touch`, …; empty when it doesn't say).
    pub pin: String,
    /// Where the host connects to it (loopback).
    pub address: SocketAddr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A screen connected, or said its name again.
    Screen(Screen),
    /// Its lobby closed: the screen went (off, or its app closed).
    Gone(String),
}

type Sink = Arc<dyn Fn(Event) + Send + Sync>;
type Ws = WebSocket<TcpStream>;

struct Lobby {
    screen: Screen,
    /// Host connections waiting for the screen to dial: n -> the host's TCP stream.
    pending: Arc<Mutex<HashMap<u32, TcpStream>>>,
    stop: Arc<AtomicBool>,
}

struct Inner {
    name: String,
    sink: Sink,
    lobbies: Mutex<Vec<Lobby>>,
    next: AtomicU32,
    stop: AtomicBool,
}

/// The bridge: listening for screens until dropped.
pub struct Bridge {
    inner: Arc<Inner>,
    port: u16,
}

impl Bridge {
    /// Listens on `bind`:`port` (0, or a port that is taken: any free port); `name` is this host's, told to each
    /// screen; `sink` hears screens come and go.
    pub fn start(bind: IpAddr, port: u16, name: &str, sink: impl Fn(Event) + Send + Sync + 'static) -> std::io::Result<Bridge> {
        let listener = TcpListener::bind((bind, port)).or_else(|_| TcpListener::bind((bind, 0)))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let inner = Arc::new(Inner { name: name.into(), sink: Arc::new(sink), lobbies: Mutex::new(Vec::new()), next: AtomicU32::new(1), stop: AtomicBool::new(false) });
        let me = inner.clone();
        std::thread::Builder::new().name("odp-out".into()).spawn(move || {
            while !me.stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let me = me.clone();
                        std::thread::spawn(move || me.accepted(stream));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        })?;
        Ok(Bridge { inner, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The screens connected now.
    pub fn screens(&self) -> Vec<Screen> {
        self.inner.lobbies.lock().unwrap().iter().map(|l| l.screen.clone()).collect()
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        for l in self.inner.lobbies.lock().unwrap().drain(..) {
            l.stop.store(true, Ordering::SeqCst);
        }
    }
}

fn is_wait(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

/// Owns `ws`: writes what arrives on `out`, reads in between (giving each message to `on`, which returns false to
/// end), until `stop`, the socket closes, or `on` says so. `tick` runs between reads; false from it ends too.
fn pump(mut ws: Ws, out: Receiver<Message>, stop: &AtomicBool, mut on: impl FnMut(Message) -> bool, mut tick: impl FnMut(&mut Ws) -> bool) {
    let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(2)));
    'run: while !stop.load(Ordering::SeqCst) {
        let mut wrote = false;
        while let Ok(m) = out.try_recv() {
            if ws.write(m).is_err() {
                break 'run;
            }
            wrote = true;
        }
        if wrote && ws.flush().is_err() {
            break;
        }
        match ws.read() {
            Ok(Message::Close(_)) => break,
            Ok(m) => {
                if !on(m) {
                    break;
                }
            }
            Err(e) if is_wait(&e) => {}
            Err(_) => break,
        }
        if !tick(&mut ws) {
            break;
        }
    }
    let _ = ws.close(None);
    let _ = ws.flush();
}

fn text(v: serde_json::Value) -> Message {
    Message::Text(v.to_string())
}

impl Inner {
    /// A WebSocket arrived: a lobby (`/odp-out`) or a dial (`/odp-out/dial/<n>`).
    fn accepted(self: Arc<Self>, stream: TcpStream) {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_nodelay(true);
        let mut path = String::new();
        let Ok(ws) = tungstenite::accept_hdr(stream, |req: &Request, resp: Response| {
            path = req.uri().path().to_string();
            Ok(resp)
        }) else {
            return;
        };
        if path == "/odp-out" {
            self.lobby(ws);
        } else if let Some(n) = path.strip_prefix("/odp-out/dial/").and_then(|n| n.parse::<u32>().ok()) {
            let host = self.lobbies.lock().unwrap().iter().find_map(|l| l.pending.lock().unwrap().remove(&n));
            match host {
                Some(h) => relay(ws, h),
                None => {
                    let mut ws = ws;
                    let _ = ws.close(None);
                    let _ = ws.flush();
                }
            }
        }
    }

    fn lobby(self: Arc<Self>, mut ws: Ws) {
        // Who it is, first (within 10 s).
        let _ = ws.get_ref().set_read_timeout(Some(Duration::from_secs(10)));
        let first = loop {
            match ws.read() {
                Ok(Message::Text(t)) => break serde_json::from_str::<serde_json::Value>(&t).ok(),
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
                _ => return,
            }
        };
        let Some(first) = first.filter(|v| v["t"] == "screen" && v["id"].is_string()) else { return };
        let Ok(loopback) = TcpListener::bind(("127.0.0.1", 0)) else { return };
        let Ok(address) = loopback.local_addr() else { return };
        let _ = loopback.set_nonblocking(true);
        let screen = screen_of(&first, address);
        let (tx, rx): (Sender<Message>, Receiver<Message>) = channel();
        let _ = tx.send(text(serde_json::json!({"t": "host", "name": self.name})));
        let pending: Arc<Mutex<HashMap<u32, TcpStream>>> = Default::default();
        let stop = Arc::new(AtomicBool::new(false));
        // The same screen again (its app restarted): the old lobby goes.
        {
            let mut lobbies = self.lobbies.lock().unwrap();
            for old in lobbies.iter().filter(|l| l.screen.id == screen.id) {
                old.stop.store(true, Ordering::SeqCst);
            }
            lobbies.retain(|l| l.screen.id != screen.id);
            lobbies.push(Lobby { screen: screen.clone(), pending: pending.clone(), stop: stop.clone() });
        }
        (self.sink)(Event::Screen(screen.clone()));
        // The host's connections to the screen's loopback address: each one a dial.
        {
            let (me, tx, pending, stop) = (self.clone(), tx.clone(), pending.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) && !me.stop.load(Ordering::SeqCst) {
                    match loopback.accept() {
                        Ok((host, _)) => {
                            let _ = host.set_nonblocking(false);
                            let _ = host.set_nodelay(true);
                            let n = me.next.fetch_add(1, Ordering::SeqCst);
                            pending.lock().unwrap().insert(n, host);
                            let _ = tx.send(text(serde_json::json!({"t": "dial", "n": n})));
                            // A dial never answered: the host's connection closes after 10 s.
                            let pending = pending.clone();
                            std::thread::spawn(move || {
                                std::thread::sleep(Duration::from_secs(10));
                                if let Some(h) = pending.lock().unwrap().remove(&n) {
                                    let _ = h.shutdown(Shutdown::Both);
                                }
                            });
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(20)),
                    }
                }
            });
        }
        // The lobby itself: names said again, pings both ways, and the end.
        let heard = std::cell::Cell::new(Instant::now());
        let mut pinged = Instant::now();
        let me = self.clone();
        let me_stop = self.clone();
        let me_stop = &me_stop.stop;
        let stop_ref = stop.clone();
        pump(
            ws,
            rx,
            &stop,
            |m| {
                heard.set(Instant::now());
                if let Message::Text(t) = m {
                    if let Some(v) = serde_json::from_str::<serde_json::Value>(&t).ok().filter(|v| v["t"] == "screen") {
                        let s = screen_of(&v, address);
                        if let Some(l) = me.lobbies.lock().unwrap().iter_mut().find(|l| Arc::ptr_eq(&l.stop, &stop_ref)) {
                            l.screen = s.clone();
                        }
                        (me.sink)(Event::Screen(s));
                    }
                }
                true
            },
            |ws| {
                if pinged.elapsed() > Duration::from_secs(10) {
                    pinged = Instant::now();
                    let _ = ws.send(Message::Ping(Vec::new()));
                }
                heard.get().elapsed() < QUIET && !me_stop.load(Ordering::SeqCst)
            },
        );
        stop.store(true, Ordering::SeqCst);
        let was_current = {
            let mut lobbies = self.lobbies.lock().unwrap();
            let before = lobbies.len();
            lobbies.retain(|l| !Arc::ptr_eq(&l.stop, &stop));
            before != lobbies.len()
        };
        if was_current {
            (self.sink)(Event::Gone(screen.id.clone()));
        }
    }
}

fn screen_of(v: &serde_json::Value, address: SocketAddr) -> Screen {
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    Screen { id: s("id"), name: s("name"), fingerprint: s("fp"), pin: s("pin"), address }
}

/// One dial: the host's TCP connection and a loopback UDP socket on one side, the screen's WebSocket on the other.
fn relay(ws: Ws, host: TcpStream) {
    let Ok(udp) = UdpSocket::bind(("127.0.0.1", 0)) else { return };
    let Ok(udp_port) = udp.local_addr().map(|a| a.port()) else { return };
    let (tx, rx) = channel::<Message>();
    let _ = tx.send(text(serde_json::json!({ "udp_port": udp_port })));
    let done = Arc::new(AtomicBool::new(false));
    // Where the host's media comes from (its sending socket), for what the screen sends back.
    let media_from: Arc<Mutex<Option<SocketAddr>>> = Default::default();
    // The host's control bytes to the screen.
    {
        let (tx, done) = (tx.clone(), done.clone());
        let Ok(mut from_host) = host.try_clone() else { return };
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            loop {
                match from_host.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut m = Vec::with_capacity(n + 1);
                        m.push(0);
                        m.extend_from_slice(&buf[..n]);
                        if tx.send(Message::Binary(m)).is_err() {
                            break;
                        }
                    }
                }
            }
            done.store(true, Ordering::SeqCst);
        });
    }
    // The host's media datagrams to the screen.
    {
        let (tx, done, media_from) = (tx.clone(), done.clone(), media_from.clone());
        let Ok(udp) = udp.try_clone() else { return };
        let _ = udp.set_read_timeout(Some(Duration::from_millis(100)));
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            while !done.load(Ordering::SeqCst) {
                if let Ok((n, from)) = udp.recv_from(&mut buf) {
                    *media_from.lock().unwrap() = Some(from);
                    let mut m = Vec::with_capacity(n + 1);
                    m.push(1);
                    m.extend_from_slice(&buf[..n]);
                    if tx.send(Message::Binary(m)).is_err() {
                        break;
                    }
                }
            }
        });
    }
    drop(tx);
    // The screen's bytes and datagrams back, until either side goes.
    let mut host_w = match host.try_clone() {
        Ok(h) => h,
        Err(_) => return,
    };
    let d = done.clone();
    pump(
        ws,
        rx,
        &AtomicBool::new(false),
        |m| match m {
            Message::Binary(b) if !b.is_empty() => match b[0] {
                0 => host_w.write_all(&b[1..]).is_ok(),
                1 => {
                    if let Some(to) = *media_from.lock().unwrap() {
                        let _ = udp.send_to(&b[1..], to);
                    }
                    true
                }
                _ => true,
            },
            _ => true,
        },
        |_| !d.load(Ordering::SeqCst),
    );
    done.store(true, Ordering::SeqCst);
    let _ = host.shutdown(Shutdown::Both);
}

#[cfg(any(test, feature = "tv"))]
pub mod tv;
#[cfg(test)]
mod tests;
