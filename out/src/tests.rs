//! A TV standing in for the LG and VIDAA apps: odp-web's receiver behind a WebSocket client, connecting out to the
//! bridge; a real host (odp::link::host) pairs with it through the bridge's loopback address, comes back as a paired
//! host, starts a session and sends a frame, which reaches the TV.

use super::*;
use odp::control::{Caps, Decode, Message as Odp, Mode};
use odp::keys::Identity;
use odp::session::Channel;
use odp_web::{Out, Web};
use std::sync::mpsc::RecvTimeoutError;

struct Tv {
    secret: [u8; 32],
    pin: Mutex<Option<String>>,
    known: Mutex<Vec<(String, [u8; 32])>>,
    frames: AtomicU32,
    events: Mutex<Vec<serde_json::Value>>,
}

fn caps() -> Caps {
    Caps { keyboard: true, decode: vec![Decode { codec: "h264".into(), max_w: 1920, max_h: 1080, max_fps: 60, hw: true }], ..Default::default() }
}

impl Tv {
    /// The lobby, and a dial for each `{"t":"dial"}`.
    fn run(self: Arc<Self>, port: u16, id: String, stop: Arc<AtomicBool>) {
        let (mut lobby, _) = tungstenite::connect(format!("ws://127.0.0.1:{port}/odp-out")).unwrap();
        let fp = Identity::from_secret(&self.secret).fingerprint();
        lobby.send(Message::Text(serde_json::json!({"t": "screen", "id": id, "name": "Lounge LG", "fp": fp, "pin": "remote"}).to_string())).unwrap();
        if let tungstenite::stream::MaybeTlsStream::Plain(s) = lobby.get_ref() {
            s.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        }
        while !stop.load(Ordering::SeqCst) {
            match lobby.read() {
                Ok(Message::Text(t)) => {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                    self.events.lock().unwrap().push(v.clone());
                    if v["t"] == "dial" {
                        let n = v["n"].as_u64().unwrap();
                        let me = self.clone();
                        std::thread::spawn(move || me.dial(port, n));
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
    fn dial(self: Arc<Self>, port: u16, n: u64) {
        let (mut ws, _) = tungstenite::connect(format!("ws://127.0.0.1:{port}/odp-out/dial/{n}")).unwrap();
        let mut web = Web::new("Lounge LG", caps(), Some(self.secret));
        for (id, key) in self.known.lock().unwrap().iter() {
            web.know(id, *key);
        }
        web.set_pin(self.pin.lock().unwrap().clone());
        web.connected();
        if let tungstenite::stream::MaybeTlsStream::Plain(s) = ws.get_ref() {
            s.set_read_timeout(Some(Duration::from_millis(2))).unwrap();
        }
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            match ws.read() {
                Ok(Message::Text(t)) => {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
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
                        let v: serde_json::Value = serde_json::from_str(&e).unwrap();
                        if v["t"] == "paired" {
                            let key = odp::link::unb64(v["key"].as_str().unwrap()).unwrap();
                            self.known.lock().unwrap().push((v["id"].as_str().unwrap().into(), key.try_into().unwrap()));
                        }
                        self.events.lock().unwrap().push(v);
                    }
                    Out::Audio { .. } => {}
                }
            }
        }
    }
}

#[test]
fn a_tv_that_connects_out_pairs_and_shows_a_frame_through_the_bridge() {
    let (tx, rx) = channel::<Event>();
    let tx = Mutex::new(tx);
    let bridge = Bridge::start([127, 0, 0, 1].into(), 0, "Test host", move |e| {
        let _ = tx.lock().unwrap().send(e);
    })
    .unwrap();
    let tv = Arc::new(Tv { secret: [9; 32], pin: Mutex::new(Some("482915".into())), known: Default::default(), frames: AtomicU32::new(0), events: Default::default() });
    let tv_id = Identity::from_secret(&tv.secret).fingerprint().replace('-', "").to_lowercase();
    let stop = Arc::new(AtomicBool::new(false));
    let lobby = {
        let (tv, stop, id) = (tv.clone(), stop.clone(), tv_id.clone());
        let port = bridge.port();
        std::thread::spawn(move || tv.run(port, id, stop))
    };

    // The screen arrives, with what it said about itself.
    let screen = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Event::Screen(s)) => s,
        other => panic!("{other:?}"),
    };
    assert_eq!((screen.id.as_str(), screen.name.as_str(), screen.pin.as_str()), (tv_id.as_str(), "Lounge LG", "remote"));
    assert!(screen.address.ip().is_loopback());
    assert_eq!(bridge.screens(), vec![screen.clone()]);

    // A real host pairs with the PIN through the bridge.
    let host_id = Identity::from_secret(&[4; 32]);
    let me = || odp::link::Me { identity: &host_id, id: "laptop", name: "Laptop", caps: Caps::default() };
    let link = odp::link::host(TcpStream::connect(screen.address).unwrap(), me(), None, Some("482915")).unwrap();
    let key = link.peer_key;
    assert_eq!(link.version(), odp::VERSION);
    drop(link);
    *tv.pin.lock().unwrap() = None;

    // Back as a paired host: a session, a frame over the loopback UDP port, and it reaches the TV.
    let mut link = odp::link::host(TcpStream::connect(screen.address).unwrap(), me(), Some(key), None).unwrap();
    link.send(&Odp::SessionStart { id: 1, mode: Mode::Mirror, codec: "h264".into(), w: 1280, h: 720, fps: 30, bitrate_kbps: 1000, audio: false, audio_codec: None, group: None, region: None }).unwrap();
    let udp_port = loop {
        match link.recv().unwrap() {
            Some(Odp::SessionOk { udp_port, .. }) => break udp_port,
            Some(_) => continue,
            None => panic!("closed"),
        }
    };
    let (mut sealer, _) = link.session.channel_keys(Channel::Media).open(Channel::Media);
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    for d in odp::media::Packetizer::new(odp::media::Kind::Video, 0).frame(&[0, 0, 0, 1, 0x65, 1, 2, 3], 1, true, &mut sealer) {
        socket.send_to(&d, ("127.0.0.1", udp_port)).unwrap();
    }
    let until = Instant::now() + Duration::from_secs(5);
    while tv.frames.load(Ordering::SeqCst) == 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(tv.frames.load(Ordering::SeqCst) > 0, "the frame reached the TV: {:?}", tv.events.lock().unwrap());
    let _ = link.send(&Odp::Bye);
    drop(link);

    // The TV goes: the host hears it.
    stop.store(true, Ordering::SeqCst);
    lobby.join().unwrap();
    let gone = loop {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Event::Gone(id)) => break id,
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => panic!("no Gone"),
        }
    };
    assert_eq!(gone, tv_id);
    assert!(bridge.screens().is_empty());
}
