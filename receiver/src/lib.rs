//! The reference ODP receiver: everything a screen needs except its decoder and its window. It listens for hosts,
//! pairs with the PIN it shows, runs a session for each host showing here (several at once: a split screen, each in
//! its own slot), keeps the shared clock, puts media back into frames and hands each one over with the moment to show
//! it, asks for a key frame when one is lost, and sends keys and the pointer back (to the focused slot). Cinema
//! messages are handed over on this screen's clock.
//!
//! A screen app (Android TV, Apple TV, a kiosk) starts a [`Receiver`] with a callback for [`Event`]s, decodes the
//! frames it gets, and calls back with keys, pointer moves, its own latency and Cinema's status. `ffi` offers the
//! same through a C interface.

pub mod ffi;
#[cfg(feature = "android")]
pub mod android;

use odp::control::{Caps, Message, PlayState};
use odp::keys::Identity;
use odp::session::{Channel, ChannelKeys, Sealer};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The ports receivers try first, in order; when all are taken, any free port.
pub const PORTS: [u16; 3] = [54330, 54331, 54332];

/// How long a pairing PIN stays on screen.
pub const PIN_LIFE: Duration = Duration::from_secs(300);

/// A host showing here that is heard from for this long (no frames, no clock answers: a laptop shut or gone off the
/// network without closing its link) is taken off the screen, so the screen doesn't keep its last picture for good.
pub const HOST_SILENT: Duration = Duration::from_secs(20);

pub struct Config {
    /// Where the identity key and the paired hosts are kept.
    pub dir: PathBuf,
    pub name: String,
    /// 0: the first free of [`PORTS`], else any free port.
    pub port: u16,
    pub caps: Caps,
    /// Nobody sits at it (a TV box, a kiosk): a paired host may rename it.
    pub kiosk: bool,
    /// The address to listen on: every interface for a screen, 127.0.0.1 for tests.
    pub bind: std::net::IpAddr,
}

/// What the screen app hears.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Listening for hosts on `port`; `fingerprint` identifies this screen.
    Listening { port: u16, fingerprint: String },
    /// The PIN to show while pairing is open; None hides it.
    Pin(Option<String>),
    Paired { host: String },
    /// A host started showing its screen here: frames follow, in `slot` (each host showing here has its own; with
    /// several at once the screen is split, one tile each, in the order they started). It is focused (`focus`).
    /// `region`: a screen of a wall shows only this part of the picture, enlarged to fill it.
    Started { slot: u32, host: String, codec: String, w: u32, h: u32, region: Option<odp::control::Region> },
    /// One encoded frame (H.264 Annex-B for "h264"), to show at `show_at_us` on this computer's clock (microseconds
    /// since 1970; 0 while the clock is still being measured: show it now). `captured_us` is when the host captured
    /// it, on this clock (0 until measured): a screen reports its delay from there to its picture with `latency`.
    Frame { slot: u32, data: Vec<u8>, show_at_us: i64, captured_us: i64, key: bool },
    /// 10 ms of sound, 16-bit PCM, 48 kHz stereo (odp L4 PCM on a LAN), to play at `play_at_us` on this computer's
    /// clock (0 while the clock is being measured: play it now). Only to screens whose caps say they play sound.
    /// With several slots, a screen plays the focused one's sound.
    Audio { slot: u32, data: Vec<u8>, play_at_us: i64 },
    Stopped { slot: u32, host: String },
    /// A paired host named this screen.
    Renamed { name: String },
    /// The monitor's other inputs changed (ODP 1.2): read them with `monitor_inputs`.
    Inputs,
    /// The owner's answer to `switch_input` (ODP 1.3): `ok`, or `text` saying why the monitor didn't switch.
    Switched { id: String, ok: bool, text: String },
    /// Cinema (L7): play `url` from its service; times are on this computer's clock.
    CinemaOpen { service: String, url: String, position_ms: i64, rate: f64, playing: bool, at_us: i64 },
    CinemaControl { position_ms: i64, rate: f64, playing: bool, at_us: i64 },
    CinemaClose,
    /// Something went wrong that the screen may say (a host that couldn't connect, …).
    Problem(String),
}

type Sink = Arc<dyn Fn(Event) + Send + Sync>;

/// Microseconds since 1970 on this computer.
pub fn now_us() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as i64).unwrap_or(0)
}

#[derive(Clone)]
struct Host {
    id: String,
    name: String,
    key: [u8; 32],
}

/// One host's link: its messages, its session's media side and its clock.
struct Slot {
    id: u32,
    /// Messages for the host.
    out: Sender<Message>,
    media: Mutex<Option<Media>>,
    /// The host's clock less this one's, once measured.
    offset: Arc<Mutex<Option<i64>>>,
    /// The group's delay from capture to picture (sync.target).
    target: Arc<AtomicI64>,
    /// How much later than the group's time this screen plays its sound (sync.offset: the host tuned the room).
    sound_offset: Arc<AtomicI64>,
    /// The decoder failed: wait for a key frame.
    spoiled: Arc<AtomicBool>,
    /// The host takes game controllers as controllers (input.controllers, ODP 1.1).
    controllers: Arc<AtomicBool>,
    /// The host's name, as presenters are listed.
    name: String,
    /// The host's id (its hello), listed in screen.showing.
    host: String,
    /// It speaks ODP 1.5 (it takes screen.showing).
    v15: bool,
    /// An owner (paired outside meeting-room mode), which may watch and end presenters (room.*, ODP 1.1).
    owner: bool,
    /// It shows a picture now.
    showing: AtomicBool,
    /// It asked for the presenters (room.watch).
    watching: AtomicBool,
    /// The monitor's other inputs, as this owner lists them (monitor.inputs, ODP 1.2).
    inputs: Mutex<Vec<odp::control::MonitorInput>>,
}

/// The session's media side, for input and the clock.
struct Media {
    socket: UdpSocket,
    input: Sealer,
    host: Arc<Mutex<Option<SocketAddr>>>,
}

struct Inner {
    cfg: Config,
    identity: Identity,
    id: String,
    name: Mutex<String>,
    hosts: Mutex<Vec<Host>>,
    pin: Mutex<Option<(String, Instant)>>,
    sink: Sink,
    /// The hosts linked here, each in its slot.
    slots: Mutex<Vec<Arc<Slot>>>,
    next_slot: AtomicU32,
    /// The slot keys, the pointer and Cinema's status go to (the last one started, unless the app says otherwise).
    focus: AtomicU32,
    /// A meeting room's screen (`set_room`): a PIN always open, each host forgotten when it leaves.
    room: AtomicBool,
    /// Game controllers' last states and how often each goes again (`pad`).
    pads: Mutex<Pads>,
    /// Hosts that paired while this was a meeting room's screen: guests, never owners.
    room_guests: Mutex<std::collections::HashSet<String>>,
    /// [`PIN_LIFE`] and [`HOST_SILENT`] (shorter in tests).
    pin_life: Mutex<Duration>,
    silent_after: Mutex<Duration>,
}

/// Each controller's last state and the sends it has left (u8::MAX: until it is at rest), and whether the resending
/// thread runs.
#[derive(Default)]
struct Pads {
    states: [Option<(odp::input::Pad, u8)>; 4],
    resending: bool,
}

/// A running receiver. Dropping it does not stop the listener (the screen app lives as long as the process).
#[derive(Clone)]
pub struct Receiver {
    inner: Arc<Inner>,
    port: u16,
}

impl Receiver {
    pub fn start(cfg: Config, sink: impl Fn(Event) + Send + Sync + 'static) -> Result<Receiver, String> {
        // (A sandboxed Mac app's folder can say it exists without letting itself be looked at as a folder: fine.)
        std::fs::create_dir_all(&cfg.dir).or_else(|e| if e.kind() == std::io::ErrorKind::AlreadyExists && !cfg.dir.is_file() { Ok(()) } else { Err(e) }).map_err(|e| format!("can't keep this screen's files in {}: {e}", cfg.dir.display()))?;
        let key_path = cfg.dir.join("identity.key");
        let identity = match std::fs::read(&key_path).ok().and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok()) {
            Some(secret) => Identity::from_secret(&secret),
            None => {
                let i = Identity::generate();
                std::fs::write(&key_path, i.secret()).map_err(|e| format!("can't save this screen's key in {}: {e}", cfg.dir.display()))?;
                i
            }
        };
        let id: String = identity.fingerprint().replace('-', "").to_lowercase();
        let hosts = load_hosts(&cfg.dir);
        let name = std::fs::read_to_string(cfg.dir.join("name")).ok().map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| cfg.name.clone());
        let listener = if cfg.port != 0 {
            TcpListener::bind((cfg.bind, cfg.port)).map_err(|e| e.to_string())?
        } else {
            // The usual ports first (easy to type), then any free one: hosts find the screen by DNS-SD, and the
            // screen shows its address and port for typing.
            let mut last = None;
            PORTS.iter().chain(&[0]).find_map(|p| TcpListener::bind((cfg.bind, *p)).map_err(|e| last = Some(e)).ok()).ok_or_else(|| {
                format!("can't listen on any port: {}", last.map(|e| e.to_string()).unwrap_or_default())
            })?
        };
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        // A screen that plays sound takes it as Opus too (decoded here, so the app still gets PCM).
        let mut cfg = cfg;
        // Owners may watch and end presenters (room.*, ODP 1.1).
        cfg.caps.room = true;
        if cfg.caps.audio_channels > 0 && !cfg.caps.audio_codecs.iter().any(|c| c == odp::audio::OPUS) {
            cfg.caps.audio_codecs.push(odp::audio::OPUS.into());
        }
        let inner = Arc::new(Inner {
            cfg,
            identity,
            id,
            name: Mutex::new(name),
            hosts: Mutex::new(hosts),
            pin: Mutex::new(None),
            sink: Arc::new(sink),
            slots: Mutex::new(Vec::new()),
            next_slot: AtomicU32::new(1),
            focus: AtomicU32::new(0),
            room: AtomicBool::new(false),
            pads: Mutex::new(Pads::default()),
            room_guests: Mutex::new(Default::default()),
            pin_life: Mutex::new(PIN_LIFE),
            silent_after: Mutex::new(HOST_SILENT),
        });
        (inner.sink)(Event::Listening { port, fingerprint: inner.identity.fingerprint() });
        let me = inner.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let me = me.clone();
                std::thread::spawn(move || serve(&me, stream));
            }
        });
        Ok(Receiver { inner, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn fingerprint(&self) -> String {
        self.inner.identity.fingerprint()
    }

    pub fn name(&self) -> String {
        self.inner.name.lock().unwrap().clone()
    }

    /// Names this screen on itself (its own settings): kept, and told to the app ([`Event::Renamed`]) so it is
    /// advertised under the new name. Empty goes back to the name the app started with (the device's own).
    pub fn rename(&self, name: &str) {
        let name: String = name.trim().chars().take(64).collect();
        let file = self.inner.cfg.dir.join("name");
        let name = if name.is_empty() {
            let _ = std::fs::remove_file(&file);
            self.inner.cfg.name.clone()
        } else {
            let _ = std::fs::write(&file, &name);
            name
        };
        *self.inner.name.lock().unwrap() = name.clone();
        (self.inner.sink)(Event::Renamed { name });
    }

    /// Opens pairing (a new PIN, shown for [`PIN_LIFE`]) or closes it. A PIN that runs out unused goes from the
    /// screen ([`Event::Pin`] None), so the screen never shows one that no longer pairs (a meeting room's is renewed
    /// instead).
    pub fn pairing(&self, open: bool) {
        let pin = open.then(odp::pair::new_pin);
        *self.inner.pin.lock().unwrap() = pin.clone().map(|p| (p, Instant::now()));
        (self.inner.sink)(Event::Pin(pin.clone()));
        if let Some(pin) = pin {
            let me = self.clone();
            let life = *self.inner.pin_life.lock().unwrap();
            std::thread::spawn(move || {
                std::thread::sleep(life);
                if me.inner.room.load(Ordering::SeqCst) {
                    return;
                }
                let mut now = me.inner.pin.lock().unwrap();
                if now.as_ref().is_some_and(|(p, _)| *p == pin) {
                    *now = None;
                    drop(now);
                    (me.inner.sink)(Event::Pin(None));
                }
            });
        }
    }

    /// Tests: a shorter PIN life and host silence.
    #[cfg(test)]
    fn set_lives(&self, pin: Duration, silent: Duration) {
        *self.inner.pin_life.lock().unwrap() = pin;
        *self.inner.silent_after.lock().unwrap() = silent;
    }

    /// A meeting room's screen, or not: while on, a PIN always shows (a new one when it is used or runs out), and
    /// each host that showed its screen here is forgotten when its link ends (guests).
    pub fn set_room(&self, on: bool) {
        let was = self.inner.room.swap(on, Ordering::SeqCst);
        if on && !was {
            self.pairing(true);
            let me = self.clone();
            std::thread::spawn(move || {
                while me.inner.room.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_secs(5));
                    let life = *me.inner.pin_life.lock().unwrap();
                    let stale = me.inner.pin.lock().unwrap().as_ref().is_none_or(|(_, t)| t.elapsed() > life.saturating_sub(Duration::from_secs(20)));
                    if stale && me.inner.room.load(Ordering::SeqCst) {
                        me.pairing(true);
                    }
                }
            });
        } else if !on && was {
            self.pairing(false);
        }
    }

    pub fn room(&self) -> bool {
        self.inner.room.load(Ordering::SeqCst)
    }

    /// Paired hosts: (id, name).
    pub fn hosts(&self) -> Vec<(String, String)> {
        self.inner.hosts.lock().unwrap().iter().map(|h| (h.id.clone(), h.name.clone())).collect()
    }

    pub fn forget(&self, id: &str) {
        let mut hosts = self.inner.hosts.lock().unwrap();
        hosts.retain(|h| h.id != id);
        save_hosts(&self.inner.cfg.dir, &hosts);
    }

    /// Which slot the keyboard, remote and pointer control (a split screen: the tile the viewer chose).
    pub fn focus(&self, slot: u32) {
        self.inner.focus.store(slot, Ordering::SeqCst);
    }

    /// Ends a host's link (`slot`): a meeting room's screen sends a presenter away. Its picture stops (`stopped`) and,
    /// in a meeting room, it is forgotten as when it leaves by itself.
    pub fn end(&self, slot: u32) {
        if let Some(s) = self.slot(slot) {
            let _ = s.out.send(Message::Bye);
        }
    }

    /// The focused slot (0: none showing).
    pub fn focused(&self) -> u32 {
        self.inner.focus.load(Ordering::SeqCst)
    }

    /// A key on this screen's keyboard or remote (USB HID usage, page 7), to the focused slot's host.
    pub fn key(&self, usage: u16, down: bool, modifiers: u8) {
        self.send(Message::Key { usage, down, modifiers });
    }

    /// The pointer, 0..65535 across the focused slot's picture.
    pub fn pointer(&self, x: u16, y: u16, buttons: u8, wheel: i16) {
        self.input(&odp::input::Event::Pointer { x, y, buttons, wheel });
    }

    /// Touch contacts on the focused slot's picture (the whole set, again every 100 ms while one is down).
    pub fn touch(&self, contacts: Vec<odp::input::Contact>) {
        self.input(&odp::input::Event::Touch(contacts));
    }

    /// A pen on the focused slot's picture: where, how hard (0..65535), its `odp::input::pen` flags.
    pub fn pen(&self, x: u16, y: u16, pressure: u16, flags: u8) {
        self.input(&odp::input::Event::Pen { x, y, pressure, flags });
    }

    /// Whether the focused slot's host takes game controllers as controllers (it said `input.controllers`): then the
    /// app sends its controller with `pad`, otherwise as keys (a remote), never both.
    /// The other inputs of the monitor this screen is on, as the owners linked here list them (ODP 1.2, when the app
    /// set `caps.inputs`), each once: (id, name).
    pub fn monitor_inputs(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for s in self.inner.slots.lock().unwrap().iter() {
            for i in s.inputs.lock().unwrap().iter() {
                if !out.iter().any(|(id, _)| *id == i.id) {
                    out.push((i.id.clone(), i.name.clone()));
                }
            }
        }
        out
    }

    /// The name of the owner that lists input `id` (the computer a `switch_input` asks), if one does.
    pub fn input_owner(&self, id: &str) -> Option<String> {
        let slots = self.inner.slots.lock().unwrap();
        slots.iter().find(|s| s.inputs.lock().unwrap().iter().any(|i| i.id == id)).map(|s| s.name.clone())
    }

    /// Asks one owner that listed it to switch the monitor to input `id` (`monitor.switch`). False: none lists it.
    pub fn switch_input(&self, id: &str) -> bool {
        let slots = self.inner.slots.lock().unwrap();
        match slots.iter().find(|s| s.inputs.lock().unwrap().iter().any(|i| i.id == id)) {
            Some(s) => s.out.send(Message::MonitorSwitch { id: id.to_string() }).is_ok(),
            None => false,
        }
    }

    pub fn controllers_taken(&self) -> bool {
        self.slot(self.focused()).is_some_and(|s| s.controllers.load(Ordering::SeqCst))
    }

    /// A game controller's whole state (ODP 1.1, input kind 4), to the focused slot's host: sent when it changes;
    /// while it isn't at rest it goes again every 100 ms, and at rest three times, so a lost datagram is repaired.
    /// Call it with each new state (polling or events, either).
    pub fn pad(&self, pad: odp::input::Pad) {
        let index = (pad.index as usize).min(3);
        let start = {
            let mut pads = self.inner.pads.lock().unwrap();
            let changed = pads.states[index].map(|(p, _)| p) != Some(pad);
            if changed {
                pads.states[index] = Some((pad, if pad.at_rest() { 3 } else { u8::MAX }));
            }
            if !changed {
                return;
            }
            let start = !pads.resending;
            pads.resending = true;
            start
        };
        self.input(&odp::input::Event::Pad(pad));
        if start {
            let me = self.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let due: Vec<odp::input::Pad> = {
                    let mut pads = me.inner.pads.lock().unwrap();
                    let mut due = Vec::new();
                    for s in pads.states.iter_mut() {
                        if let Some((p, left)) = s {
                            if *left > 0 {
                                due.push(*p);
                                if *left != u8::MAX {
                                    *left -= 1;
                                }
                            }
                        }
                    }
                    // Nothing more to send: this ends until the next change.
                    if due.is_empty() {
                        pads.resending = false;
                        return;
                    }
                    due
                };
                for p in due {
                    me.input(&odp::input::Event::Pad(p));
                }
            });
        }
    }

    fn input(&self, e: &odp::input::Event) {
        let Some(slot) = self.slot(self.focused()) else { return };
        if let Some(m) = slot.media.lock().unwrap().as_mut() {
            if let Some(to) = *m.host.lock().unwrap() {
                let _ = m.socket.send_to(&odp::input::seal(e, &mut m.input), to);
            }
        };
    }

    /// The decoder of `slot` couldn't use a frame: its next frame is a key frame.
    pub fn decoder_failed(&self, slot: u32) {
        if let Some(s) = self.slot(slot) {
            s.spoiled.store(true, Ordering::SeqCst);
        }
    }

    /// This screen's sound output changed (`device`: its name; `kind`: hdmi, speaker, bluetooth, airplay, usb,
    /// headphones; `reported_us`: the latency the system states for it): every host showing here times it again.
    pub fn output(&self, device: &str, kind: &str, reported_us: i64) {
        for s in self.inner.slots.lock().unwrap().iter() {
            let _ = s.out.send(Message::SyncOutput { device: device.into(), kind: kind.into(), reported_us });
        }
    }

    /// The delay from a frame's capture time to its picture in `slot`, in microseconds (sync.latency).
    pub fn latency(&self, slot: u32, video_us: i64) {
        if let Some(s) = self.slot(slot) {
            let _ = s.out.send(Message::SyncLatency { video_us, audio_us: 0, source: "measured".into() });
        }
    }

    /// Cinema on this screen: where the title is and whether it plays, or why it can't.
    pub fn cinema_status(&self, position_ms: i64, playing: bool, error: Option<String>) {
        let Some(slot) = self.slot(self.focused()) else { return };
        let at_us = now_us() + slot.offset.lock().unwrap().unwrap_or(0);
        let state = if playing { PlayState::Playing } else { PlayState::Paused };
        let _ = slot.out.send(Message::CinemaStatus { position_ms, at_us, state, error });
    }

    fn slot(&self, id: u32) -> Option<Arc<Slot>> {
        self.inner.slots.lock().unwrap().iter().find(|s| s.id == id).cloned()
    }

    fn send(&self, m: Message) {
        if let Some(s) = self.slot(self.focused()) {
            let _ = s.out.send(m);
        }
    }
}

/// Who shows here now, to each owner watching (room.presenters, ODP 1.1).
fn tell_presenters(inner: &Arc<Inner>) {
    let slots = inner.slots.lock().unwrap();
    let list: Vec<odp::control::Presenter> = slots.iter().filter(|s| s.showing.load(Ordering::SeqCst)).map(|s| odp::control::Presenter { slot: s.id, name: s.name.clone(), id: None }).collect();
    for s in slots.iter().filter(|s| s.owner && s.watching.load(Ordering::SeqCst)) {
        let _ = s.out.send(Message::RoomPresenters { list: list.clone() });
    }
    // Every host that speaks 1.5 hears who shows, with ids (screen.showing): they decide who combines.
    let showing: Vec<odp::control::Presenter> = slots.iter().filter(|s| s.showing.load(Ordering::SeqCst)).map(|s| odp::control::Presenter { slot: s.id, name: s.name.clone(), id: Some(s.host.clone()) }).collect();
    for s in slots.iter().filter(|s| s.v15) {
        let _ = s.out.send(Message::ScreenShowing { list: showing.clone() });
    }
}

fn load_hosts(dir: &std::path::Path) -> Vec<Host> {
    let Ok(text) = std::fs::read_to_string(dir.join("hosts.json")) else { return Vec::new() };
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str(&text) else { return Vec::new() };
    items
        .iter()
        .filter_map(|h| {
            let key = hex32(h["key"].as_str()?)?;
            Some(Host { id: h["id"].as_str()?.to_string(), name: h["name"].as_str().unwrap_or_default().to_string(), key })
        })
        .collect()
}

fn save_hosts(dir: &std::path::Path, hosts: &[Host]) {
    let list: Vec<serde_json::Value> = hosts
        .iter()
        .map(|h| serde_json::json!({"id": h.id, "name": h.name, "key": h.key.iter().map(|b| format!("{b:02x}")).collect::<String>()}))
        .collect();
    let _ = std::fs::write(dir.join("hosts.json"), serde_json::Value::Array(list).to_string());
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    let bytes: Vec<u8> = (0..s.len()).step_by(2).filter_map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect();
    bytes.try_into().ok()
}

/// One host connecting: pairs if it's new and the PIN is up, then runs its control messages.
fn serve(inner: &Arc<Inner>, stream: TcpStream) {
    let from = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let hosts = inner.hosts.lock().unwrap().clone();
    let life = *inner.pin_life.lock().unwrap();
    let pin = inner.pin.lock().unwrap().clone().filter(|(_, t)| t.elapsed() < life).map(|(p, _)| p);
    let paired = |id: &str| hosts.iter().find(|h| h.id == id).map(|h| h.key);
    let paired_before = |id: &str, key: &[u8; 32]| hosts.iter().any(|h| h.id == id && h.key == *key);
    let name = inner.name.lock().unwrap().clone();
    let me = odp::link::Me { identity: &inner.identity, id: &inner.id, name: &name, caps: inner.cfg.caps.clone() };
    // A new host is stored the moment pairing succeeds (before the session exchange), so it can reconnect at once.
    let stored = |id: &str, name: &str, key: &[u8; 32]| {
        let mut hosts = inner.hosts.lock().unwrap();
        hosts.retain(|h| h.id != id);
        hosts.push(Host { id: id.to_string(), name: name.to_string(), key: *key });
        save_hosts(&inner.cfg.dir, &hosts);
    };
    let mut link = match odp::link::receiver(stream, me, odp::link::ReceiverPolicy { paired: &paired, pin, on_paired: Some(&stored) }) {
        Ok(l) => l,
        Err(e) => {
            if let Some(text) = connect_problem(&from, &e) {
                (inner.sink)(Event::Problem(text));
            }
            return;
        }
    };
    {
        // Paired just now (stored when pairing succeeded): the PIN goes.
        if inner.pin.lock().unwrap().is_some() && !paired_before(&link.peer_id, &link.peer_key) {
            *inner.pin.lock().unwrap() = None;
            (inner.sink)(Event::Pin(None));
            (inner.sink)(Event::Paired { host: link.peer_name.clone() });
            // A meeting room: a guest (never an owner), and the next person's PIN at once.
            if inner.room.load(Ordering::SeqCst) {
                inner.room_guests.lock().unwrap().insert(link.peer_id.clone());
                let pin = odp::pair::new_pin();
                *inner.pin.lock().unwrap() = Some((pin.clone(), Instant::now()));
                (inner.sink)(Event::Pin(Some(pin)));
            }
        }
    }
    let (tx, rx) = channel::<Message>();
    let slot = Arc::new(Slot {
        id: inner.next_slot.fetch_add(1, Ordering::SeqCst),
        out: tx.clone(),
        media: Mutex::new(None),
        offset: Arc::new(Mutex::new(None)),
        target: Arc::new(AtomicI64::new(0)),
        sound_offset: Arc::new(AtomicI64::new(0)),
        spoiled: Arc::new(AtomicBool::new(false)),
        controllers: Arc::new(AtomicBool::new(false)),
        name: link.peer_name.clone(),
        host: link.peer_id.clone(),
        v15: link.peer_versions.iter().any(|v| v == "odp/1.5"),
        owner: !inner.room_guests.lock().unwrap().contains(&link.peer_id),
        showing: AtomicBool::new(false),
        watching: AtomicBool::new(false),
        inputs: Mutex::new(Vec::new()),
    });
    inner.slots.lock().unwrap().push(slot.clone());
    // Keys go to a linked host before it shows anything too (none other is focused).
    let _ = inner.focus.compare_exchange(0, slot.id, Ordering::SeqCst, Ordering::SeqCst);
    let mut stop_media: Option<Arc<AtomicBool>> = None;
    // Whether this link showed a session (a meeting room forgets such a guest when it goes).
    let mut showed = false;
    let local = |offset: &Arc<Mutex<Option<i64>>>, host_us: i64| host_us - offset.lock().unwrap().unwrap_or(0);
    loop {
        // Messages for the host; a Bye (`end`) ends the link once sent.
        let mut ending = false;
        while let Ok(m) = rx.try_recv() {
            ending |= m == Message::Bye;
            if link.send(&m).is_err() || ending {
                break;
            }
        }
        if ending {
            break;
        }
        // A short wait, so keys and key frame requests reach the host without delay.
        let _ = link.stream().set_read_timeout(Some(Duration::from_millis(15)));
        let mut probe = [0u8; 1];
        match link.stream().peek(&mut probe) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(_) => break,
        }
        let _ = link.stream().set_read_timeout(Some(Duration::from_secs(30)));
        match link.recv() {
            Ok(None) | Ok(Some(Message::Bye)) => break,
            Ok(Some(Message::Controllers { on })) => slot.controllers.store(on, Ordering::SeqCst),
            // An owner watching who shows (ODP 1.1); a guest is told no.
            Ok(Some(Message::RoomWatch { on })) => {
                if slot.owner {
                    slot.watching.store(on, Ordering::SeqCst);
                    if on {
                        tell_presenters(inner);
                    }
                } else {
                    let _ = link.send(&Message::Error { id: None, code: "not_owner".into(), text: "Only a computer paired with this screen outside a meeting can see who shows.".into() });
                }
            }
            // An owner on the same monitor listing its other inputs (ODP 1.2); a guest's list is ignored.
            Ok(Some(Message::MonitorInputs { list })) => {
                if slot.owner {
                    *slot.inputs.lock().unwrap() = list;
                    (inner.sink)(Event::Inputs);
                }
            }
            // The owner's answer to a switch this screen asked for (ODP 1.3).
            Ok(Some(Message::MonitorSwitched { id, ok, text })) => {
                if slot.owner {
                    (inner.sink)(Event::Switched { id, ok, text });
                }
            }
            // An owner ending a presenter.
            Ok(Some(Message::RoomEnd { slot: which })) => {
                if slot.owner {
                    if let Some(s) = inner.slots.lock().unwrap().iter().find(|s| s.id == which) {
                        let _ = s.out.send(Message::Bye);
                    }
                } else {
                    let _ = link.send(&Message::Error { id: None, code: "not_owner".into(), text: "Only a computer paired with this screen outside a meeting can end a presenter.".into() });
                }
            }
            Ok(Some(Message::SessionStart { id, codec, w, h, region, audio_codec, .. })) => {
                let opus = audio_codec.as_deref() == Some(odp::audio::OPUS);
                if let Some(s) = stop_media.take() {
                    s.store(true, Ordering::SeqCst);
                }
                let socket = match UdpSocket::bind((inner.cfg.bind, 0)) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = link.send(&Message::Error { id: Some(id), code: "no_viewer".into(), text: e.to_string() });
                        continue;
                    }
                };
                let udp_port = socket.local_addr().map(|a| a.port()).unwrap_or(0);
                let stop = Arc::new(AtomicBool::new(false));
                let media_keys = link.session.channel_keys(Channel::Media);
                let input_keys = link.session.channel_keys(Channel::Input);
                start_media(inner, &slot, socket, media_keys, input_keys, stop.clone(), opus);
                stop_media = Some(stop);
                showed = true;
                inner.focus.store(slot.id, Ordering::SeqCst);
                let _ = link.send(&Message::SessionOk { id, udp_port });
                (inner.sink)(Event::Started { slot: slot.id, host: link.peer_name.clone(), codec, w, h, region });
                slot.showing.store(true, Ordering::SeqCst);
                tell_presenters(inner);
            }
            Ok(Some(Message::SessionStop)) => {
                if let Some(s) = stop_media.take() {
                    s.store(true, Ordering::SeqCst);
                    (inner.sink)(Event::Stopped { slot: slot.id, host: link.peer_name.clone() });
                }
                slot.showing.store(false, Ordering::SeqCst);
                tell_presenters(inner);
            }
            Ok(Some(Message::SyncTarget { delay_us })) => slot.target.store(delay_us, Ordering::SeqCst),
            Ok(Some(Message::SyncOffset { us })) => slot.sound_offset.store(us.clamp(-500_000, 500_000), Ordering::SeqCst),
            Ok(Some(Message::Rename { name })) => {
                if inner.cfg.kiosk {
                    let name: String = name.trim().chars().take(64).collect();
                    let _ = std::fs::write(inner.cfg.dir.join("name"), &name);
                    *inner.name.lock().unwrap() = name.clone();
                    (inner.sink)(Event::Renamed { name: name.clone() });
                    let _ = link.send(&Message::Rename { name });
                } else {
                    let _ = link.send(&Message::Error { id: None, code: "not_kiosk".into(), text: "This screen is named on itself.".into() });
                }
            }
            Ok(Some(Message::CinemaOpen { service, url, position_ms, rate, state, at_us })) => {
                let at_us = local(&slot.offset, at_us);
                (inner.sink)(Event::CinemaOpen { service, url, position_ms, rate, playing: state == PlayState::Playing, at_us });
            }
            Ok(Some(Message::CinemaControl { state, position_ms, rate, at_us })) => {
                let at_us = local(&slot.offset, at_us);
                (inner.sink)(Event::CinemaControl { position_ms, rate, playing: state == PlayState::Playing, at_us });
            }
            Ok(Some(Message::CinemaClose)) => (inner.sink)(Event::CinemaClose),
            Ok(Some(_)) => {}
            Err(e) => {
                (inner.sink)(Event::Problem(format!("The connection to {} was lost ({e}).", link.peer_name)));
                break;
            }
        }
    }
    if let Some(s) = stop_media.take() {
        s.store(true, Ordering::SeqCst);
        (inner.sink)(Event::Stopped { slot: slot.id, host: link.peer_name.clone() });
    }
    {
        let mut slots = inner.slots.lock().unwrap();
        slots.retain(|s| s.id != slot.id);
        // The focus moves to the last one still here.
        if inner.focus.load(Ordering::SeqCst) == slot.id {
            inner.focus.store(slots.last().map_or(0, |s| s.id), Ordering::SeqCst);
        }
    }
    if slot.showing.load(Ordering::SeqCst) {
        tell_presenters(inner);
    }
    // A meeting room: the guest is forgotten once it has shown its screen and left (not after the short link that
    // only paired it, which ends before it casts).
    if inner.room.load(Ordering::SeqCst) && showed {
        let mut hosts = inner.hosts.lock().unwrap();
        hosts.retain(|h| h.id != link.peer_id);
        save_hosts(&inner.cfg.dir, &hosts);
    }
}

/// What a screen may say when a computer couldn't connect, in plain words. None: nothing worth a line on the screen
/// (a connection closed before it said anything: a computer checking the screen is there, or one that gave up).
fn connect_problem(from: &str, e: &str) -> Option<String> {
    let who = if from.is_empty() { "A computer".to_string() } else { format!("A computer at {from}") };
    Some(match e {
        "the other side closed the connection" => return None,
        "not pairing" => format!("{who} tried to pair while no PIN was showing."),
        "the PIN doesn't match" => format!("{who} typed a PIN that doesn't match. Type the PIN shown here."),
        "an unpaired host that didn't start pairing" => format!("{who} isn't paired with this screen any more. Add it again with a PIN."),
        _ => format!("{who} couldn't connect ({e})."),
    })
}

/// After a lost frame, only a key frame may go to the decoder; asks the host for one (at most every 250 ms).
#[derive(Default)]
struct KeyGate {
    asked: Option<Instant>,
    last: Option<u32>,
    lost: bool,
    started: bool,
}

impl KeyGate {
    /// Whether frame `frame` may be decoded; `ask` is called when a key frame is needed.
    fn admit(&mut self, frame: u32, key: bool, ask: impl FnOnce()) -> bool {
        if self.last.is_some_and(|l| frame.wrapping_sub(l) != 1) {
            self.lost = true;
        }
        self.last = Some(frame);
        if (self.lost || !self.started) && !key {
            if self.asked.is_none_or(|t| t.elapsed() > Duration::from_millis(250)) {
                ask();
                self.asked = Some(Instant::now());
            }
            return false;
        }
        self.lost = false;
        self.started = true;
        true
    }
}

/// The session's UDP side: clock pings, frames, and the socket and sealer input goes out with.
fn start_media(inner: &Arc<Inner>, slot: &Arc<Slot>, socket: UdpSocket, media: ChannelKeys, input: ChannelKeys, stop: Arc<AtomicBool>, opus: bool) {
    let host: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
    if let Ok(clone) = socket.try_clone() {
        *slot.media.lock().unwrap() = Some(Media { socket: clone, input: input.open(Channel::Input).0, host: host.clone() });
    }
    let (sink, offset, target, spoiled) = (inner.sink.clone(), slot.offset.clone(), slot.target.clone(), slot.spoiled.clone());
    let (sound_offset, out, id) = (slot.sound_offset.clone(), slot.out.clone(), slot.id);
    let (silent_after, name) = (*inner.silent_after.lock().unwrap(), slot.name.clone());
    *offset.lock().unwrap() = None;
    std::thread::spawn(move || {
        // When the host was last heard from (a frame or a clock answer).
        let mut heard = Instant::now();
        let (mut pinger, mut opener) = media.open(Channel::Media);
        let mut clock = odp::clock::Clock::default();
        let mut pinged = Instant::now() - Duration::from_secs(2);
        let _ = socket.set_read_timeout(Some(Duration::from_millis(200)));
        let mut re = odp::media::Reassembler::default();
        let mut gate = KeyGate::default();
        let mut sound = Sound::new(opus);
        let mut buf = vec![0u8; 2048];
        while !stop.load(Ordering::SeqCst) {
            let known = *host.lock().unwrap();
            // Gone without a word (shut, asleep, off the network): its picture comes off the screen.
            if known.is_some() && heard.elapsed() > silent_after {
                sink(Event::Problem(format!("{name} stopped answering, so its picture was taken off this screen.")));
                let _ = out.send(Message::Bye);
                break;
            }
            if let Some(to) = known.filter(|_| pinged.elapsed() >= Duration::from_secs(1)) {
                let _ = socket.send_to(&odp::clock::Probe::Ping { t1: now_us() }.seal(&mut pinger), to);
                pinged = Instant::now();
            }
            let Ok((n, from)) = socket.recv_from(&mut buf) else { continue };
            if buf[..n].starts_with(odp::clock::MAGIC) {
                if let Some(odp::clock::Probe::Pong { t1, t2, t3 }) = odp::clock::Probe::open(&buf[..n], &mut opener) {
                    heard = Instant::now();
                    clock.add(odp::clock::Sample { t1, t2, t3, t4: now_us() });
                    *offset.lock().unwrap() = clock.offset();
                }
                continue;
            }
            let done = re.push(&buf[..n], &mut opener);
            // Lost packets are asked for again at once (L4 media.nack): a frame can still complete in time.
            let missing = re.take_missing();
            if !missing.is_empty() {
                let _ = out.send(Message::Nack { stream: 0, seqs: missing });
            }
            let Some(frame) = done else { continue };
            heard = Instant::now();
            // Only a packet that opened says where the host is.
            if known != Some(from) {
                *host.lock().unwrap() = Some(from);
            }
            if frame.kind == odp::media::Kind::Audio {
                let captured = clock.to_local(frame.pts_us).unwrap_or(0);
                let play_at_us = if captured > 0 { captured + target.load(Ordering::SeqCst) + sound_offset.load(Ordering::SeqCst) } else { 0 };
                for (data, at) in sound.take(frame.frame, &frame.data, play_at_us) {
                    sink(Event::Audio { slot: id, data, play_at_us: at });
                }
                continue;
            }
            if spoiled.swap(false, Ordering::SeqCst) {
                gate.lost = true;
            }
            if !gate.admit(frame.frame, frame.key, || {
                let _ = out.send(Message::Keyframe);
            }) {
                continue;
            }
            let captured_us = clock.to_local(frame.pts_us).unwrap_or(0);
            let show_at_us = if captured_us > 0 { captured_us + target.load(Ordering::SeqCst) } else { 0 };
            sink(Event::Frame { slot: id, data: frame.data, show_at_us, captured_us, key: frame.key });
        }
    });
}

/// Stream 1's frames as PCM for the app: PCM as it comes; Opus decoded, a lost frame rebuilt from the next one's copy
/// (in-band FEC) or concealed, so the sound has no holes.
struct Sound {
    opus: Option<odp::audio::Decoder>,
    last: Option<u32>,
}

impl Sound {
    fn new(opus: bool) -> Sound {
        Sound { opus: opus.then(|| odp::audio::Decoder::new(2).ok()).flatten(), last: None }
    }

    /// Frames to play, each with its time (a frame is 10 ms).
    fn take(&mut self, frame: u32, data: &[u8], at: i64) -> Vec<(Vec<u8>, i64)> {
        let gap = self.last.map_or(1, |l| frame.wrapping_sub(l));
        if gap == 0 || gap > u32::MAX / 2 {
            return Vec::new(); // a repeat or a late frame: already played over
        }
        self.last = Some(frame);
        let Some(dec) = self.opus.as_mut() else { return vec![(data.to_vec(), at)] };
        let mut out = Vec::new();
        let missing = (gap - 1).min(5) as i64; // beyond 50 ms the player starts again anyway
        let shift = |k: i64| if at > 0 { at - k * 10_000 } else { 0 };
        for k in (2..=missing).rev() {
            if let Ok(pcm) = dec.conceal() {
                out.push((odp::audio::to_bytes(&pcm), shift(k)));
            }
        }
        if missing >= 1 {
            if let Ok(pcm) = dec.recover(data) {
                out.push((odp::audio::to_bytes(&pcm), shift(1)));
            }
        }
        if let Ok(pcm) = dec.decode(data) {
            out.push((odp::audio::to_bytes(&pcm), at));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A meeting room's screen shows a PIN at once and keeps it; off, the PIN goes.
    #[test]
    fn a_meeting_room_always_shows_a_pin() {
        let dir = std::env::temp_dir().join(format!("odp-room-{}", std::process::id()));
        let (tx, rx) = std::sync::mpsc::channel::<Event>();
        let tx = Mutex::new(tx);
        let bind = std::net::IpAddr::from([127, 0, 0, 1]);
        let r = Receiver::start(Config { dir: dir.clone(), name: "Room 2".into(), port: 0, caps: Caps::default(), kiosk: true, bind }, move |e| {
            let _ = tx.lock().unwrap().send(e);
        })
        .unwrap();
        r.set_room(true);
        assert!(r.room());
        let pin = rx.iter().find_map(|e| match e {
            Event::Pin(p) => Some(p),
            _ => None,
        });
        assert!(matches!(pin, Some(Some(ref p)) if p.len() == 6), "{pin:?}");
        r.set_room(false);
        let gone = rx.iter().find_map(|e| match e {
            Event::Pin(p) => Some(p),
            _ => None,
        });
        assert_eq!(gone, Some(None));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Named on the screen itself: kept across restarts, told to the app, and empty goes back to the device's name.
    #[test]
    fn a_screen_renamed_on_itself_keeps_its_name() {
        let bind = std::net::IpAddr::from([127, 0, 0, 1]);
        let dir = std::env::temp_dir().join(format!("odp-receiver-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = Mutex::new(tx);
        let r = Receiver::start(Config { dir: dir.clone(), name: "Living Room".into(), port: 0, caps: Caps::default(), kiosk: true, bind }, move |e| {
            let _ = tx.lock().unwrap().send(e);
        })
        .unwrap();
        r.rename("  Boardroom TV  ");
        assert_eq!(r.name(), "Boardroom TV");
        let told = rx.iter().find_map(|e| match e {
            Event::Renamed { name } => Some(name),
            _ => None,
        });
        assert_eq!(told.as_deref(), Some("Boardroom TV"));
        drop(r);
        let again = Receiver::start(Config { dir: dir.clone(), name: "Living Room".into(), port: 0, caps: Caps::default(), kiosk: true, bind }, |_| {}).unwrap();
        assert_eq!(again.name(), "Boardroom TV");
        again.rename("");
        assert_eq!(again.name(), "Living Room");
        assert!(!dir.join("name").exists());
        drop(again);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// With 54330-54332 taken, a receiver still listens (on any free port), and a bad folder says what went wrong.
    #[test]
    fn a_receiver_listens_when_its_usual_ports_are_taken() {
        let bind = std::net::IpAddr::from([127, 0, 0, 1]);
        let _held: Vec<_> = PORTS.iter().filter_map(|p| TcpListener::bind((bind, *p)).ok()).collect();
        let dir = std::env::temp_dir().join(format!("odp-receiver-ports-{}", std::process::id()));
        let r = Receiver::start(Config { dir: dir.clone(), name: "TV".into(), port: 0, caps: Caps::default(), kiosk: true, bind }, |_| {});
        let port = r.as_ref().map(|r| r.port()).expect("listening");
        assert!(port != 0 && !PORTS.contains(&port), "{port}");
        assert!(TcpStream::connect((bind, port)).is_ok());
        // A file where its folder should be.
        let file = dir.join("identity.key");
        let e = Receiver::start(Config { dir: file, name: "TV".into(), port: 0, caps: Caps::default(), kiosk: true, bind }, |_| {}).err().unwrap();
        assert!(e.contains("can't keep this screen's files"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An owner on the same monitor lists its other inputs (ODP 1.2); the screen reads them and asks for one.
    #[test]
    fn an_owner_lists_the_monitors_inputs_and_takes_a_switch() {
        let dir = std::env::temp_dir().join(format!("odp-receiver-inputs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, rx) = channel::<Event>();
        let tx = Mutex::new(tx);
        let bind = std::net::IpAddr::from([127, 0, 0, 1]);
        let caps = Caps { inputs: true, ..Default::default() };
        let r = Receiver::start(Config { dir: dir.clone(), name: "Xbox".into(), port: 0, caps, kiosk: true, bind }, move |e| {
            let _ = tx.lock().unwrap().send(e);
        })
        .unwrap();
        r.pairing(true);
        let pin = loop {
            if let Event::Pin(Some(p)) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                break p;
            }
        };
        let id = Identity::generate();
        let me = odp::link::Me { identity: &id, id: "pc", name: "Ben's PC", caps: Caps::default() };
        let mut host = odp::link::host(TcpStream::connect(("127.0.0.1", r.port())).unwrap(), me, None, Some(&pin)).unwrap();
        assert!(host.peer_caps.inputs, "the screen says it lists inputs");
        let list = vec![odp::control::MonitorInput { id: "pc".into(), name: "Ben's PC".into() }, odp::control::MonitorInput { id: "input:17".into(), name: "TV box".into() }];
        host.send(&Message::MonitorInputs { list }).unwrap();
        assert!(rx.iter().any(|e| e == Event::Inputs));
        assert_eq!(r.monitor_inputs(), vec![("pc".to_string(), "Ben's PC".to_string()), ("input:17".to_string(), "TV box".to_string())]);
        assert!(r.switch_input("input:17"));
        assert!(!r.switch_input("nothing"));
        let _ = host.stream().set_read_timeout(Some(Duration::from_secs(5)));
        let asked = loop {
            match host.recv().unwrap() {
                Some(Message::MonitorSwitch { id }) => break id,
                Some(_) => continue,
                None => panic!("closed"),
            }
        };
        assert_eq!(asked, "input:17");
        assert_eq!(host.version(), odp::VERSION, "both speak 1.3");
        host.send(&Message::MonitorSwitched { id: "input:17".into(), ok: false, text: "TV box: the monitor didn't answer.".into() }).unwrap();
        assert!(rx.iter().any(|e| e == Event::Switched { id: "input:17".into(), ok: false, text: "TV box: the monitor didn't answer.".into() }));
        drop(r);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A host that speaks ODP 1.5 hears who shows here, with ids (screen.showing): computers decide who combines.
    #[test]
    fn a_host_that_speaks_1_5_hears_who_shows_with_ids() {
        let dir = std::env::temp_dir().join(format!("odp-receiver-showing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, rx) = channel::<Event>();
        let tx = Mutex::new(tx);
        let bind = std::net::IpAddr::from([127, 0, 0, 1]);
        let caps = Caps { streams: 1, ..Caps::default() };
        let r = Receiver::start(Config { dir: dir.clone(), name: "TV".into(), port: 0, caps, kiosk: true, bind }, move |e| {
            let _ = tx.lock().unwrap().send(e);
        })
        .unwrap();
        r.pairing(true);
        let pin = loop {
            if let Event::Pin(Some(p)) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                break p;
            }
        };
        let id = Identity::generate();
        let me = odp::link::Me { identity: &id, id: "alpha-node", name: "Alpha", caps: Caps::default() };
        let mut host = odp::link::host(TcpStream::connect(("127.0.0.1", r.port())).unwrap(), me, None, Some(&pin)).unwrap();
        assert_eq!(host.peer_caps.streams, 1, "the screen says how many pictures it plays");
        let _ = host.stream().set_read_timeout(Some(Duration::from_secs(5)));
        host.send(&Message::SessionStart { id: 1, mode: odp::control::Mode::Mirror, codec: "h264".into(), w: 640, h: 360, fps: 30, bitrate_kbps: 2000, audio: false, audio_codec: None, group: None, region: None }).unwrap();
        let list = loop {
            if let Some(Message::ScreenShowing { list }) = host.recv().unwrap() {
                if !list.is_empty() {
                    break list;
                }
            }
        };
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id.as_deref(), Some("alpha-node"));
        drop(host);
        drop(r);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Two hosts show here at once (a split screen): their frames arrive in two slots, and keys go to the focused one.
    /// An owner (paired outside a meeting) watches who shows and ends a presenter; a meeting room's guest can't.
    #[test]
    fn an_owner_sees_who_shows_and_ends_them() {
        let dir = std::env::temp_dir().join(format!("odp-receiver-owner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, rx) = channel::<Event>();
        let tx = Mutex::new(tx);
        let bind = std::net::IpAddr::from([127, 0, 0, 1]);
        let r = Receiver::start(Config { dir: dir.clone(), name: "Room 2".into(), port: 0, caps: Caps::default(), kiosk: true, bind }, move |e| {
            let _ = tx.lock().unwrap().send(e);
        })
        .unwrap();
        let pin_now = |rx: &std::sync::mpsc::Receiver<Event>| loop {
            if let Event::Pin(Some(p)) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                break p;
            }
        };
        let connect = |host: &str, name: &str, pin: &str| {
            let id = Identity::generate();
            let me = odp::link::Me { identity: &id, id: host, name, caps: Caps::default() };
            odp::link::host(TcpStream::connect(("127.0.0.1", r.port())).unwrap(), me, None, Some(pin)).unwrap()
        };
        // The owner pairs before the room opens, and says it takes room.* (caps.room).
        r.pairing(true);
        let pin = pin_now(&rx);
        let mut owner = connect("owner", "Facilities", &pin);
        assert!(owner.peer_caps.room, "the screen says it takes room.*");
        let _ = owner.stream().set_read_timeout(Some(Duration::from_secs(5)));
        owner.send(&Message::RoomWatch { on: true }).unwrap();
        let first = loop {
            if let Some(Message::RoomPresenters { list }) = owner.recv().unwrap() {
                break list;
            }
        };
        assert!(first.is_empty(), "nobody shows yet");
        // A meeting: a guest joins with the room's PIN and shows.
        r.set_room(true);
        let pin = pin_now(&rx);
        let mut guest = connect("guest", "Ana's laptop", &pin);
        guest.send(&Message::SessionStart { id: 1, mode: odp::control::Mode::Mirror, codec: "h264".into(), w: 640, h: 360, fps: 30, bitrate_kbps: 2000, audio: false, audio_codec: None, group: None, region: None }).unwrap();
        let _ = guest.stream().set_read_timeout(Some(Duration::from_secs(5)));
        assert!(matches!(guest.recv().unwrap(), Some(Message::SessionOk { .. })));
        let shows = loop {
            if let Some(Message::RoomPresenters { list }) = owner.recv().unwrap() {
                if !list.is_empty() {
                    break list;
                }
            }
        };
        assert_eq!(shows.len(), 1);
        assert_eq!(shows[0].name, "Ana's laptop");
        // The guest may not watch.
        guest.send(&Message::RoomWatch { on: true }).unwrap();
        let refused = loop {
            match guest.recv().unwrap() {
                Some(Message::Error { code, .. }) => break code,
                Some(_) => continue,
                None => panic!("closed"),
            }
        };
        assert_eq!(refused, "not_owner");
        // The owner ends the guest's presentation: the guest hears Bye; the list empties.
        owner.send(&Message::RoomEnd { slot: shows[0].slot }).unwrap();
        let mut said_bye = false;
        while let Ok(Some(m)) = guest.recv() {
            if m == Message::Bye {
                said_bye = true;
                break;
            }
        }
        assert!(said_bye, "the guest wasn't sent away");
        let after = loop {
            if let Some(Message::RoomPresenters { list }) = owner.recv().unwrap() {
                break list;
            }
        };
        assert!(after.is_empty(), "{after:?}");
        r.set_room(false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_hosts_show_at_once_in_their_own_slots() {
        let dir = std::env::temp_dir().join(format!("odp-receiver-slots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, rx) = channel::<Event>();
        let tx = Mutex::new(tx);
        let caps = Caps { keyboard: true, ..Default::default() };
        let bind = std::net::IpAddr::from([127, 0, 0, 1]);
        let r = Receiver::start(Config { dir: dir.clone(), name: "Room TV".into(), port: 0, caps, kiosk: true, bind }, move |e| {
            let _ = tx.lock().unwrap().send(e);
        })
        .unwrap();
        let mut links = Vec::new();
        let mut started = Vec::new();
        for (n, name) in ["Laptop A", "Laptop B"].iter().enumerate() {
            r.pairing(true);
            let pin = loop {
                if let Event::Pin(Some(p)) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                    break p;
                }
            };
            let id = Identity::generate();
            let host = format!("host{n}");
            let me = odp::link::Me { identity: &id, id: &host, name, caps: Caps::default() };
            let mut link = odp::link::host(TcpStream::connect(("127.0.0.1", r.port())).unwrap(), me, None, Some(&pin)).unwrap();
            link.send(&Message::SessionStart { id: 1, mode: odp::control::Mode::Mirror, codec: "h264".into(), w: 640, h: 360, fps: 30, bitrate_kbps: 2000, audio: false, audio_codec: None, group: None, region: None }).unwrap();
            let udp_port = match link.recv().unwrap().unwrap() {
                Message::SessionOk { udp_port, .. } => udp_port,
                m => panic!("{m:?}"),
            };
            let slot = loop {
                if let Event::Started { slot, .. } = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                    break slot;
                }
            };
            // A key frame marked with whose it is.
            let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
            let (mut sealer, _) = link.session.channel_keys(Channel::Media).open(Channel::Media);
            let mut p = odp::media::Packetizer::new(odp::media::Kind::Video, 0);
            for d in p.frame(&[0, 0, 0, 1, n as u8], now_us(), true, &mut sealer) {
                udp.send_to(&d, ("127.0.0.1", udp_port)).unwrap();
            }
            let from = loop {
                if let Event::Frame { slot, data, .. } = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                    break (slot, data[4]);
                }
            };
            assert_eq!(from, (slot, n as u8));
            started.push(slot);
            links.push(link);
        }
        assert_ne!(started[0], started[1]);
        // The last one started has the keys; focusing the first sends them there.
        assert_eq!(r.focused(), started[1]);
        r.focus(started[0]);
        r.key(0x28, true, 0);
        let _ = links[0].stream().set_read_timeout(Some(Duration::from_secs(5)));
        loop {
            if let Message::Key { usage, .. } = links[0].recv().unwrap().unwrap() {
                assert_eq!(usage, 0x28);
                break;
            }
        }
        // The first leaves: its slot stops, and the keys move to the one still showing.
        drop(links.remove(0));
        let gone = loop {
            if let Event::Stopped { slot, .. } = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                break slot;
            }
        };
        assert_eq!(gone, started[0]);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(r.focused(), started[1]);
        // The room's screen sends the other one away: its host hears Bye, and its slot stops.
        r.end(started[1]);
        let _ = links[0].stream().set_read_timeout(Some(Duration::from_secs(5)));
        let mut said_bye = false;
        while let Ok(Some(m)) = links[0].recv() {
            if m == Message::Bye {
                said_bye = true;
                break;
            }
        }
        assert!(said_bye, "the host wasn't told");
        let gone = loop {
            if let Event::Stopped { slot, .. } = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                break slot;
            }
        };
        assert_eq!(gone, started[1]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A host pairs with the PIN on screen, starts a session, sends one key frame and two more, and gets a key back.
    #[test]
    fn a_host_pairs_and_its_frames_arrive_in_order() {
        let dir = std::env::temp_dir().join(format!("odp-receiver-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, rx) = channel::<Event>();
        let tx = Mutex::new(tx);
        let caps = Caps { keyboard: true, pointer: true, ..Default::default() };
        let bind = std::net::IpAddr::from([127, 0, 0, 1]);
        let r = Receiver::start(Config { dir: dir.clone(), name: "Test TV".into(), port: 0, caps, kiosk: true, bind }, move |e| {
            let _ = tx.lock().unwrap().send(e);
        })
        .or_else(|_| Err::<Receiver, String>("no port".into()))
        .unwrap();
        r.pairing(true);
        let pin = loop {
            if let Event::Pin(Some(p)) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                break p;
            }
        };
        // The host side, as Omninput does it.
        let host_id = Identity::generate();
        let stream = TcpStream::connect(("127.0.0.1", r.port())).unwrap();
        let me = odp::link::Me { identity: &host_id, id: "host1", name: "Laptop", caps: Caps::default() };
        let mut link = odp::link::host(stream, me, None, Some(&pin)).unwrap();
        assert_eq!(link.peer_name, "Test TV");
        link.send(&Message::SessionStart { id: 1, mode: odp::control::Mode::Mirror, codec: "h264".into(), w: 1280, h: 720, fps: 30, bitrate_kbps: 4000, audio: true, audio_codec: Some("opus".into()), group: None, region: None }).unwrap();
        let udp_port = match link.recv().unwrap().unwrap() {
            Message::SessionOk { udp_port, .. } => udp_port,
            m => panic!("{m:?}"),
        };
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let (mut sealer, _) = link.session.channel_keys(Channel::Media).open(Channel::Media);
        let mut p = odp::media::Packetizer::new(odp::media::Kind::Video, 0);
        for (i, key) in [(0u8, true), (1, false), (2, false)] {
            let data = vec![0, 0, 0, 1, i, 9, 9, 9];
            for d in p.frame(&data, now_us(), key, &mut sealer) {
                udp.send_to(&d, ("127.0.0.1", udp_port)).unwrap();
            }
        }
        let mut frames = Vec::new();
        let until = Instant::now() + Duration::from_secs(5);
        while frames.len() < 3 && Instant::now() < until {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Event::Frame { data, key, .. }) => frames.push((data[4], key)),
                Ok(Event::Paired { host }) => assert_eq!(host, "Laptop"),
                _ => {}
            }
        }
        assert_eq!(frames, vec![(0, true), (1, false), (2, false)]);
        // Sound on its own stream, as Opus (the session asked for it), comes through as PCM: six frames sent with the
        // fourth lost on the way, six frames heard (the lost one rebuilt from the fifth's copy).
        let mut sound = odp::media::Packetizer::new(odp::media::Kind::Audio, 1);
        let mut enc = odp::audio::Encoder::new(2, 96_000).unwrap();
        for f in 0..6 {
            let pcm: Vec<i16> = (0..odp::audio::FRAME).flat_map(|i| {
                let v = (((f * odp::audio::FRAME + i) as f32 * 0.0576).sin() * 9000.0) as i16;
                [v, v]
            }).collect();
            let packet = enc.encode(&pcm).unwrap();
            let datagrams = sound.frame(&packet, now_us(), false, &mut sealer);
            if f == 3 {
                continue;
            }
            for d in datagrams {
                udp.send_to(&d, ("127.0.0.1", udp_port)).unwrap();
            }
        }
        let until = Instant::now() + Duration::from_secs(5);
        let mut heard = 0;
        while heard < 6 && Instant::now() < until {
            if let Ok(Event::Audio { data, .. }) = rx.recv_timeout(Duration::from_millis(500)) {
                assert_eq!(data.len(), 1920);
                heard += 1;
            }
        }
        assert_eq!(heard, 6, "frames heard");
        // A key from the screen's remote reaches the host.
        r.key(0x28, true, 0);
        let _ = link.stream().set_read_timeout(Some(Duration::from_secs(5)));
        loop {
            match link.recv().unwrap().unwrap() {
                Message::Key { usage, down, .. } => {
                    assert_eq!((usage, down), (0x28, true));
                    break;
                }
                _ => continue,
            }
        }
        // A game controller (ODP 1.1): its state reaches the host, again every 100 ms while held; at rest it goes
        // four times (once, then three more) and stops.
        assert!(!r.controllers_taken(), "keys until the host says it takes controllers");
        link.send(&Message::Controllers { on: true }).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while !r.controllers_taken() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(r.controllers_taken());
        let (_, mut opener) = link.session.channel_keys(Channel::Input).open(Channel::Input);
        let _ = udp.set_read_timeout(Some(Duration::from_millis(400)));
        let held = odp::input::Pad { index: 0, buttons: odp::input::pad::A, lx: 20000, ..Default::default() };
        r.pad(held);
        r.pad(held); // the same again: nothing new is sent for it
        std::thread::sleep(Duration::from_millis(350));
        r.pad(odp::input::Pad::default());
        let mut got = Vec::new();
        let mut buf = [0u8; 256];
        while let Ok((n, _)) = udp.recv_from(&mut buf) {
            if let Some(odp::input::Event::Pad(p)) = odp::input::open(&buf[..n], &mut opener) {
                got.push(p.at_rest());
            }
        }
        let held_sends = got.iter().filter(|r| !**r).count();
        assert!((3..=5).contains(&held_sends), "held state sent {held_sends} times: {got:?}");
        assert_eq!(got.iter().filter(|r| **r).count(), 4, "{got:?}");
        assert!(got.last() == Some(&true));
        // Paired: it's remembered.
        assert_eq!(r.hosts(), vec![("host1".to_string(), "Laptop".to_string())]);
        assert!(dir.join("hosts.json").exists());
        drop(link);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A PIN that runs out goes from the screen (it would no longer pair), unless a newer one replaced it.
    #[test]
    fn a_pin_that_runs_out_goes_from_the_screen() {
        let dir = std::env::temp_dir().join(format!("odp-receiver-pin-life-{}", std::process::id()));
        let (tx, rx) = channel::<Event>();
        let tx = Mutex::new(tx);
        let bind = std::net::IpAddr::from([127, 0, 0, 1]);
        let r = Receiver::start(Config { dir: dir.clone(), name: "TV".into(), port: 0, caps: Caps::default(), kiosk: true, bind }, move |e| {
            let _ = tx.lock().unwrap().send(e);
        })
        .unwrap();
        r.set_lives(Duration::from_millis(300), HOST_SILENT);
        r.pairing(true);
        let pins: Vec<Option<String>> = rx.iter().filter_map(|e| if let Event::Pin(p) = e { Some(p) } else { None }).take(2).collect();
        assert!(pins[0].is_some() && pins[1].is_none(), "{pins:?}");
        // A new PIN before the old one ran out: only the newest one's end takes it away.
        r.pairing(true);
        std::thread::sleep(Duration::from_millis(150));
        r.pairing(true);
        let mut after = Vec::new();
        while let Ok(Event::Pin(p)) = rx.recv_timeout(Duration::from_millis(600)) {
            after.push(p);
        }
        assert_eq!(after.len(), 3, "{after:?}");
        assert!(after[0].is_some() && after[1].is_some() && after[1] != after[0] && after[2].is_none(), "{after:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A host that goes quiet while showing (shut, asleep, off the network, its link never closed) is taken off the
    /// screen: `stopped` comes, with a problem saying why.
    #[test]
    fn a_host_that_goes_quiet_is_taken_off_the_screen() {
        let dir = std::env::temp_dir().join(format!("odp-receiver-quiet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, rx) = channel::<Event>();
        let tx = Mutex::new(tx);
        let bind = std::net::IpAddr::from([127, 0, 0, 1]);
        let r = Receiver::start(Config { dir: dir.clone(), name: "TV".into(), port: 0, caps: Caps::default(), kiosk: true, bind }, move |e| {
            let _ = tx.lock().unwrap().send(e);
        })
        .unwrap();
        r.set_lives(PIN_LIFE, Duration::from_millis(1500));
        r.pairing(true);
        let pin = loop {
            if let Event::Pin(Some(p)) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                break p;
            }
        };
        let host_id = Identity::generate();
        let stream = TcpStream::connect(("127.0.0.1", r.port())).unwrap();
        let me = odp::link::Me { identity: &host_id, id: "host1", name: "Laptop", caps: Caps::default() };
        let mut link = odp::link::host(stream, me, None, Some(&pin)).unwrap();
        link.send(&Message::SessionStart { id: 1, mode: odp::control::Mode::Mirror, codec: "h264".into(), w: 1280, h: 720, fps: 30, bitrate_kbps: 4000, audio: false, audio_codec: None, group: None, region: None }).unwrap();
        let udp_port = match link.recv().unwrap().unwrap() {
            Message::SessionOk { udp_port, .. } => udp_port,
            m => panic!("{m:?}"),
        };
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let (mut sealer, _) = link.session.channel_keys(Channel::Media).open(Channel::Media);
        let mut p = odp::media::Packetizer::new(odp::media::Kind::Video, 0);
        for d in p.frame(&[0, 0, 0, 1, 0, 9, 9, 9], now_us(), true, &mut sealer) {
            udp.send_to(&d, ("127.0.0.1", udp_port)).unwrap();
        }
        // Then nothing: no frames, no clock answers, and the link left open.
        let started = Instant::now();
        let (mut problem, mut stopped) = (None, false);
        while !stopped && started.elapsed() < Duration::from_secs(10) {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Event::Problem(t)) => problem = Some(t),
                Ok(Event::Stopped { host, .. }) => {
                    assert_eq!(host, "Laptop");
                    stopped = true;
                }
                _ => {}
            }
        }
        assert!(stopped, "still showing after {:?}", started.elapsed());
        assert!(started.elapsed() >= Duration::from_millis(1400), "too soon: {:?}", started.elapsed());
        assert!(problem.is_some_and(|t| t.contains("Laptop stopped answering")));
        drop(link);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn connection_problems_read_as_plain_words() {
        assert_eq!(connect_problem("192.168.1.5", "the PIN doesn't match").as_deref(), Some("A computer at 192.168.1.5 typed a PIN that doesn't match. Type the PIN shown here."));
        assert!(connect_problem("192.168.1.5", "not pairing").is_some_and(|t| t.contains("while no PIN was showing")));
        assert_eq!(connect_problem("192.168.1.5", "the other side closed the connection"), None);
    }
}
