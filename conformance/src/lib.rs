//! A conformance host for ODP receivers (SPEC.md, "Conformance"). It plays the host against a receiver on the
//! network and reports each check, by level:
//!
//! - **Basic**: refuses a wrong PIN, pairs with the right one, takes a paired host back without a PIN, answers
//!   `session.start`, pings the clock while media arrives, asks for a key frame after a lost frame, survives a
//!   replayed packet.
//! - **Synced**: reports its delay (`sync.latency`), which needs it to decode and show the frames.
//! - **Interactive**: a key pressed at the receiver arrives as `input.key` (only when asked to wait for one).
//! - **Cinema**: answers `cinema.open` with `cinema.status`.
//!
//! The video is 30 frames of H.264 colour bars (one key frame), sent in a loop at 30 frames a second.

use odp::control::{Caps, Message, Mode};
use odp::keys::Identity;
use odp::link::{host, Link, Me};
use odp::session::Channel;
use std::net::{TcpStream, UdpSocket};
use std::time::{Duration, Instant};

/// The test video: each frame is a u32 big-endian length, a key byte (1: key frame) and the Annex B access unit.
pub const BARS: &[u8] = include_bytes!("../bars.frames");

pub fn frames() -> Vec<(bool, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 5 <= BARS.len() {
        let n = u32::from_be_bytes(BARS[i..i + 4].try_into().unwrap()) as usize;
        out.push((BARS[i + 4] == 1, BARS[i + 5..i + 5 + n].to_vec()));
        i += 5 + n;
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Basic,
    Synced,
    Interactive,
    Cinema,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Fail(String),
    Skipped(String),
}

#[derive(Clone, Debug)]
pub struct Check {
    pub level: Level,
    pub name: &'static str,
    pub outcome: Outcome,
}

#[derive(Default, Debug)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    fn add(&mut self, level: Level, name: &'static str, outcome: Outcome) {
        self.checks.push(Check { level, name, outcome });
    }

    /// Whether every check of `level` ran and passed.
    pub fn meets(&self, level: Level) -> bool {
        let of: Vec<_> = self.checks.iter().filter(|c| c.level == level).collect();
        !of.is_empty() && of.iter().all(|c| c.outcome == Outcome::Pass)
    }

    pub fn text(&self) -> String {
        let mut s = String::new();
        for c in &self.checks {
            let (mark, why) = match &c.outcome {
                Outcome::Pass => ("pass", String::new()),
                Outcome::Fail(w) => ("FAIL", format!(": {w}")),
                Outcome::Skipped(w) => ("skip", format!(": {w}")),
            };
            s.push_str(&format!("{mark}  {:<11} {}{why}\n", format!("{:?}", c.level), c.name));
        }
        let levels = [Level::Basic, Level::Synced, Level::Interactive, Level::Cinema];
        let met: Vec<String> = levels.iter().filter(|l| self.meets(**l)).map(|l| format!("{l:?}")).collect();
        s.push_str(&format!("ODP 0.1: {}\n", if met.is_empty() { "no level met".into() } else { met.join(", ") }));
        s
    }
}

pub struct Config {
    /// The receiver, `address:port`.
    pub address: String,
    /// The PIN it shows now.
    pub pin: String,
    /// Wait this long for a key pressed at the receiver (0: skip the Interactive check).
    pub key_wait: Duration,
    /// Cast the test video this long.
    pub seconds: u64,
}

pub fn now_us() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as i64).unwrap_or(0)
}

/// Runs every check against the receiver.
pub fn run(cfg: &Config) -> Report {
    let mut r = Report::default();
    let identity = Identity::generate();
    let id = format!("odp-check-{}", &identity.fingerprint()[..4]);
    let me = || Me { identity: &identity, id: &id, name: "ODP conformance host", caps: Caps::default() };
    let connect = || TcpStream::connect(&cfg.address).map_err(|e| e.to_string());

    // A wrong PIN fails (the receiver locks pairing only after three).
    let wrong = if cfg.pin == "000000" { "111111" } else { "000000" };
    r.add(Level::Basic, "refuses a wrong PIN", match connect().and_then(|s| host(s, me(), None, Some(wrong))) {
        Ok(_) => Outcome::Fail("paired with a wrong PIN".into()),
        Err(_) => Outcome::Pass,
    });

    // The right PIN pairs.
    let (key, caps) = match connect().and_then(|s| host(s, me(), None, Some(&cfg.pin))) {
        Ok(link) => {
            r.add(Level::Basic, "pairs with the PIN it shows", Outcome::Pass);
            (link.peer_key, link.peer_caps.clone())
        }
        Err(e) => {
            r.add(Level::Basic, "pairs with the PIN it shows", Outcome::Fail(e));
            return r;
        }
    };
    r.add(Level::Basic, "decodes H.264 (hello.caps)", if caps.decode.iter().any(|d| d.codec == "h264") { Outcome::Pass } else { Outcome::Fail("no h264 in caps.decode".into()) });

    // Back as a paired host, no PIN.
    let mut link = match connect().and_then(|s| host(s, me(), Some(key), None)) {
        Ok(l) => {
            r.add(Level::Basic, "takes a paired host back without a PIN", Outcome::Pass);
            l
        }
        Err(e) => {
            r.add(Level::Basic, "takes a paired host back without a PIN", Outcome::Fail(e));
            return r;
        }
    };
    let _ = link.stream().set_read_timeout(Some(Duration::from_millis(5)));

    // A session.
    if let Err(e) = link.send(&Message::SessionStart { id: 1, mode: Mode::Mirror, codec: "h264".into(), w: 1280, h: 720, fps: 30, bitrate_kbps: 4000, audio: false, audio_codec: None, group: None, region: None }) {
        r.add(Level::Basic, "answers session.start", Outcome::Fail(e));
        return r;
    }
    let udp_port = match wait_for(&mut link, Duration::from_secs(5), |m| match m {
        Message::SessionOk { udp_port, .. } => Some(*udp_port),
        _ => None,
    }) {
        Some(p) => {
            r.add(Level::Basic, "answers session.start", Outcome::Pass);
            p
        }
        None => {
            r.add(Level::Basic, "answers session.start", Outcome::Fail("no session.ok within 5 s".into()));
            return r;
        }
    };
    let target = std::net::SocketAddr::new(link.stream().peer_addr().map(|a| a.ip()).unwrap_or([127, 0, 0, 1].into()), udp_port);
    cast(&mut r, &mut link, target, cfg);
    let _ = link.send(&Message::SessionStop);
    let _ = link.send(&Message::Bye);
    r
}

/// The media part: the test video in a loop, clock pongs, a lost frame, a replayed packet, and what comes back.
fn cast(r: &mut Report, link: &mut Link<TcpStream>, target: std::net::SocketAddr, cfg: &Config) {
    let video = frames();
    // A receiver on this computer: loopback only (no firewall prompt for a test); else every interface.
    let local: std::net::IpAddr = if target.ip().is_loopback() { [127, 0, 0, 1].into() } else { [0, 0, 0, 0].into() };
    let socket = match UdpSocket::bind((local, 0)) {
        Ok(s) => s,
        Err(e) => return r.add(Level::Basic, "media socket", Outcome::Fail(e.to_string())),
    };
    let _ = socket.set_nonblocking(true);
    let (mut sealer, mut opener) = link.session.channel_keys(Channel::Media).open(Channel::Media);
    let mut packets = odp::media::Packetizer::new(odp::media::Kind::Video, 0);
    let (mut pinged, mut asked_key, mut latency, mut key_pressed, mut cinema) = (false, None::<Instant>, false, false, false);
    let mut dropped_at: Option<Instant> = None;
    let mut replay: Option<Vec<u8>> = None;
    let mut replayed = false;
    let started = Instant::now();
    let mut next = 0usize;
    let mut tick = Instant::now();
    let total = Duration::from_secs(cfg.seconds.max(8)) + cfg.key_wait;
    if !cfg.key_wait.is_zero() {
        eprintln!("Press a key on the receiver's keyboard or remote in the next {} s.", total.as_secs());
    }
    let mut cinema_sent = false;
    let mut key_requests: Vec<u64> = Vec::new();
    let mut held: Option<(u32, Vec<u8>, Instant)> = None;
    let mut nacked: Option<Duration> = None;
    let mut nacks: Vec<Vec<u32>> = Vec::new();
    let mut buf = vec![0u8; 2048];
    while started.elapsed() < total {
        // The video, 30 frames a second, from the key frame again after a loss.
        if tick.elapsed() >= Duration::from_millis(33) {
            tick = Instant::now();
            let (key, data) = &video[next % video.len()];
            // At 3 s, one frame is lost on purpose: the receiver should ask for a key frame.
            let datagrams = packets.frame(data, now_us(), *key, &mut sealer);
            // Lost: it takes its frame number but never arrives.
            // Not just before a key frame: the receiver would rightly start again from that one without asking.
            if dropped_at.is_none() && started.elapsed() > Duration::from_secs(3) && !key && !video[(next + 1) % video.len()].0 {
                dropped_at = Some(Instant::now());
            } else {
                // At 2 s, one packet from the middle of a key frame is held back: the receiver should name it in a
                // media.nack, and gets it then.
                let mut datagrams = datagrams;
                if held.is_none() && *key && datagrams.len() >= 3 && started.elapsed() > Duration::from_secs(2) {
                    let d = datagrams.remove(1);
                    let seq = odp::media::Header::parse(&d).map(|h| h.seq).unwrap_or(0);
                    held = Some((seq, d, Instant::now()));
                }
                for d in datagrams {
                    if replay.is_none() {
                        replay = Some(d.clone());
                    }
                    let _ = socket.send_to(&d, target);
                }
            }
            next += 1;
            // After 5 s, an old packet again: it must be ignored, not shown nor end the session.
            if !replayed && started.elapsed() > Duration::from_secs(5) {
                if let Some(old) = &replay {
                    let _ = socket.send_to(old, target);
                }
                replayed = true;
            }
            // Cinema last: a receiver may hand the screen to a film app, which would take the key check's key.
            if !cinema_sent && started.elapsed() > Duration::from_secs(4) && (cfg.key_wait.is_zero() || key_pressed || started.elapsed() > total - Duration::from_secs(4)) {
                let _ = link.send(&Message::CinemaOpen { service: "example.com".into(), url: "https://example.com/odp-check".into(), position_ms: 0, rate: 1.0, state: odp::control::PlayState::Paused, at_us: now_us() });
                cinema_sent = true;
            }
        }
        // Clock pings: answered at once.
        while let Ok((n, from)) = socket.recv_from(&mut buf) {
            if let Some(odp::clock::Probe::Ping { t1 }) = odp::clock::Probe::open(&buf[..n], &mut opener) {
                pinged = true;
                let t = now_us();
                let _ = socket.send_to(&odp::clock::Probe::Pong { t1, t2: t, t3: now_us() }.seal(&mut sealer), from);
            }
        }
        // Control messages back.
        let mut probe = [0u8; 1];
        if matches!(link.stream().peek(&mut probe), Ok(n) if n > 0) {
            let _ = link.stream().set_read_timeout(Some(Duration::from_secs(5)));
            match link.recv() {
                Ok(Some(Message::Keyframe)) => {
                    key_requests.push(started.elapsed().as_millis() as u64);
                    if dropped_at.is_some() && asked_key.is_none() {
                        asked_key = Some(Instant::now());
                        next = 0; // the key frame
                    }
                }
                Ok(Some(Message::SyncLatency { video_us, .. })) if video_us > 0 => latency = true,
                Ok(Some(Message::Nack { seqs, .. })) => {
                    if let Some((seq, d, at)) = &held {
                        if nacked.is_none() && seqs.contains(seq) {
                            nacked = Some(at.elapsed());
                            let _ = socket.send_to(d, target);
                        }
                    }
                    nacks.push(seqs);
                }
                Ok(Some(Message::Key { .. })) => key_pressed = true,
                Ok(Some(Message::CinemaStatus { .. })) => cinema = true,
                Ok(Some(Message::Error { code, .. })) if code == "not_cinema" => cinema = true,
                Ok(None) | Err(_) => break,
                _ => {}
            }
            let _ = link.stream().set_read_timeout(Some(Duration::from_millis(5)));
        }
        if latency && pinged && asked_key.is_some() && nacked.is_some() && cinema && (cfg.key_wait.is_zero() || key_pressed) && replayed {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    if cinema_sent {
        let _ = link.send(&Message::CinemaClose);
    }
    let alive = link.send(&Message::SyncTarget { delay_us: 0 }).is_ok();
    r.add(Level::Basic, "pings the clock while media arrives", if pinged { Outcome::Pass } else { Outcome::Fail("no clock.ping".into()) });
    r.add(Level::Basic, "asks for a key frame after a lost frame", match (dropped_at, asked_key) {
        (Some(d), Some(a)) if a.duration_since(d) < Duration::from_secs(2) => Outcome::Pass,
        (Some(_), Some(_)) => Outcome::Fail("asked, but more than 2 s after the loss".into()),
        _ => Outcome::Fail(format!("no session.keyframe after the loss at {} ms (requests at {key_requests:?} ms)", dropped_at.map(|d| d.duration_since(started).as_millis()).unwrap_or(0))),
    });
    r.add(Level::Basic, "asks for a lost packet again (media.nack)", match (&held, nacked) {
        (Some(_), Some(t)) if t < Duration::from_millis(500) => Outcome::Pass,
        (Some(_), Some(t)) => Outcome::Fail(format!("asked after {} ms", t.as_millis())),
        (Some((seq, _, _)), None) => Outcome::Fail(format!("no media.nack for packet {seq} (nacks: {nacks:?})")),
        (None, _) => Outcome::Skipped("no key frame was sent in three packets or more".into()),
    });
    r.add(Level::Basic, "ignores a replayed packet", if alive { Outcome::Pass } else { Outcome::Fail("the session ended".into()) });
    r.add(Level::Synced, "reports its delay (sync.latency)", if latency { Outcome::Pass } else { Outcome::Fail("no sync.latency".into()) });
    r.add(Level::Interactive, "a key pressed at the receiver arrives", match (cfg.key_wait.is_zero(), key_pressed) {
        (true, _) => Outcome::Skipped("not asked to wait for a key (--key)".into()),
        (false, true) => Outcome::Pass,
        (false, false) => Outcome::Fail("no input.key".into()),
    });
    r.add(Level::Cinema, "answers cinema.open", if cinema { Outcome::Pass } else if caps_cinema_absent(link) { Outcome::Skipped("no cinema in its caps".into()) } else { Outcome::Fail("no cinema.status".into()) });
}

fn caps_cinema_absent(link: &Link<TcpStream>) -> bool {
    link.peer_caps.cinema.is_empty()
}

/// Cinema against a real service (`odp-check ... --cinema URL`): opens the title at 30 s playing, pauses it at 45 s,
/// then plays from 90 s, printing each `cinema.status`; returns the statuses as (seconds since start, position_ms,
/// playing, error).
pub fn cinema_follow(cfg: &Config, url: &str) -> Result<Vec<(f64, i64, bool, Option<String>)>, String> {
    let identity = Identity::generate();
    let id = format!("odp-check-{}", &identity.fingerprint()[..4]);
    let me = Me { identity: &identity, id: &id, name: "ODP conformance host", caps: Caps::default() };
    let mut link = host(TcpStream::connect(&cfg.address).map_err(|e| e.to_string())?, me, None, Some(&cfg.pin))?;
    let started = Instant::now();
    let mut out = Vec::new();
    use odp::control::PlayState::{Paused, Playing};
    let steps: [(u64, Message); 3] = [
        (0, Message::CinemaOpen { service: "youtube".into(), url: url.into(), position_ms: 30_000, rate: 1.0, state: Playing, at_us: now_us() + 3_000_000 }),
        (20, Message::CinemaControl { state: Paused, position_ms: 45_000, rate: 1.0, at_us: now_us() + 20_000_000 }),
        (30, Message::CinemaControl { state: Playing, position_ms: 90_000, rate: 1.0, at_us: now_us() + 30_000_000 }),
    ];
    let mut next = 0;
    let mut probe = [0u8; 1];
    while started.elapsed() < Duration::from_secs(42) {
        if next < steps.len() && started.elapsed() >= Duration::from_secs(steps[next].0) {
            link.send(&steps[next].1)?;
            next += 1;
        }
        let _ = link.stream().set_read_timeout(Some(Duration::from_millis(100)));
        if matches!(link.stream().peek(&mut probe), Ok(n) if n > 0) {
            let _ = link.stream().set_read_timeout(Some(Duration::from_secs(5)));
            if let Ok(Some(Message::CinemaStatus { position_ms, state, error, .. })) = link.recv() {
                let t = started.elapsed().as_secs_f64();
                println!("{t:5.1} s  {:>7} ms  {state:?}{}", position_ms, error.as_deref().map(|e| format!("  ({e})")).unwrap_or_default());
                out.push((t, position_ms, state == Playing, error));
            }
        }
    }
    let _ = link.send(&Message::CinemaClose);
    let _ = link.send(&Message::Bye);
    Ok(out)
}

/// Shows the test video on a receiver for `seconds` (a demonstration, or several hosts at once to try a split
/// screen): pairs with the PIN, starts a session and sends the colour bars in a loop, from a key frame whenever the
/// receiver asks. `name` is the host's name on the receiver.
pub fn show(cfg: &Config, name: &str, seconds: u64) -> Result<(), String> {
    let identity = Identity::generate();
    let id = format!("odp-check-{}", &identity.fingerprint()[..4]);
    let me = Me { identity: &identity, id: &id, name, caps: Caps::default() };
    let mut link = host(TcpStream::connect(&cfg.address).map_err(|e| e.to_string())?, me, None, Some(&cfg.pin))?;
    // Sound too when the receiver plays it: a short quiet beep each second, as Opus when it takes it.
    let audio = link.peer_caps.audio_channels > 0;
    let opus = audio && link.peer_caps.audio_codecs.iter().any(|c| c == odp::audio::OPUS);
    let audio_codec = audio.then(|| if opus { odp::audio::OPUS } else { odp::audio::PCM }.to_string());
    if let Some(c) = &audio_codec {
        println!("sound: {c}");
    }
    link.send(&Message::SessionStart { id: 1, mode: Mode::Mirror, codec: "h264".into(), w: 1280, h: 720, fps: 30, bitrate_kbps: 4000, audio, audio_codec, group: None, region: None })?;
    let udp_port = wait_for(&mut link, Duration::from_secs(5), |m| match m {
        Message::SessionOk { udp_port, .. } => Some(*udp_port),
        _ => None,
    })
    .ok_or("no session.ok")?;
    let target = std::net::SocketAddr::new(link.stream().peer_addr().map_err(|e| e.to_string())?.ip(), udp_port);
    let local: std::net::IpAddr = if target.ip().is_loopback() { [127, 0, 0, 1].into() } else { [0, 0, 0, 0].into() };
    let socket = UdpSocket::bind((local, 0)).map_err(|e| e.to_string())?;
    let _ = socket.set_nonblocking(true);
    let (mut sealer, _) = link.session.channel_keys(Channel::Media).open(Channel::Media);
    // What the screen sends back (pointer, touch, pen) is printed, one line each.
    let (_, mut input) = link.session.channel_keys(Channel::Input).open(Channel::Input);
    let mut back = [0u8; 2048];
    let mut packets = odp::media::Packetizer::new(odp::media::Kind::Video, 0);
    let mut sound = odp::media::Packetizer::new(odp::media::Kind::Audio, 1);
    let mut encoder = if opus { Some(odp::audio::Encoder::new(2, 128_000)?) } else { None };
    let video = frames();
    let started = Instant::now();
    let start_us = now_us();
    let mut sent_sound = 0i64;
    let mut next = 0usize;
    let mut probe = [0u8; 1];
    while started.elapsed() < Duration::from_secs(seconds) {
        let (key, data) = &video[next % video.len()];
        for d in packets.frame(data, now_us(), *key, &mut sealer) {
            let _ = socket.send_to(&d, target);
        }
        next += 1;
        // The sound frames due by now (10 ms each).
        while audio && sent_sound * 10_000 < now_us() - start_us {
            let pcm = beep(sent_sound);
            let data = match encoder.as_mut() {
                Some(e) => e.encode(&pcm)?,
                None => odp::audio::to_bytes(&pcm),
            };
            for d in sound.frame(&data, start_us + sent_sound * 10_000, false, &mut sealer) {
                let _ = socket.send_to(&d, target);
            }
            sent_sound += 1;
        }
        while let Ok((n, _)) = socket.recv_from(&mut back) {
            if let Some(e) = odp::input::open(&back[..n], &mut input) {
                println!("input {e:?}");
            }
        }
        // A key frame asked for: the loop starts again from its first frame (a key frame).
        let _ = link.stream().set_read_timeout(Some(Duration::from_millis(33)));
        if matches!(link.stream().peek(&mut probe), Ok(n) if n > 0) {
            let _ = link.stream().set_read_timeout(Some(Duration::from_secs(5)));
            match link.recv() {
                Ok(Some(Message::Keyframe)) => next = 0,
                Ok(Some(Message::Key { usage, down, .. })) => println!("key {usage} {down}"),
                Ok(None) | Ok(Some(Message::Bye)) | Err(_) => break,
                _ => {}
            }
        }
    }
    let _ = link.send(&Message::SessionStop);
    let _ = link.send(&Message::Bye);
    Ok(())
}

/// Sound frame `n` (10 ms, stereo): a 660 Hz beep, quiet, for the first 150 ms of each second; silence otherwise.
fn beep(n: i64) -> Vec<i16> {
    let in_second = (n % 100) as usize;
    (0..odp::audio::FRAME)
        .flat_map(|i| {
            let t = in_second * odp::audio::FRAME + i;
            let v = if in_second < 15 { ((t as f32 * 660.0 * std::f32::consts::TAU / 48_000.0).sin() * 2_500.0) as i16 } else { 0 };
            [v, v]
        })
        .collect()
}

/// The first message `want` accepts within `time` (others are passed over).
fn wait_for<T>(link: &mut Link<TcpStream>, time: Duration, want: impl Fn(&Message) -> Option<T>) -> Option<T> {
    let until = Instant::now() + time;
    let mut probe = [0u8; 1];
    while Instant::now() < until {
        // Wait for a message to start (a timeout inside one would lose the framing), then read it whole.
        let _ = link.stream().set_read_timeout(Some(Duration::from_millis(200)));
        if !matches!(link.stream().peek(&mut probe), Ok(n) if n > 0) {
            continue;
        }
        let _ = link.stream().set_read_timeout(Some(Duration::from_secs(5)));
        match link.recv() {
            Ok(Some(m)) => {
                if let Some(v) = want(&m) {
                    return Some(v);
                }
            }
            _ => return None,
        }
    }
    None
}
