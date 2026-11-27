//! A TV standing in for the LG and VIDAA apps: odp-web's receiver behind a WebSocket client, connecting out to the
//! bridge; a real host (odp::link::host) pairs with it through the bridge's loopback address, comes back as a paired
//! host, starts a session and sends a frame, which reaches the TV.

use super::*;
use crate::tv::Tv;
use odp::control::{Caps, Message as Odp, Mode};
use odp::keys::Identity;
use odp::session::Channel;
use std::sync::mpsc::RecvTimeoutError;

#[test]
fn a_tv_that_connects_out_pairs_and_shows_a_frame_through_the_bridge() {
    let (tx, rx) = channel::<Event>();
    let tx = Mutex::new(tx);
    let bridge = Bridge::start([127, 0, 0, 1].into(), 0, "Test host", move |e| {
        let _ = tx.lock().unwrap().send(e);
    })
    .unwrap();
    let tv = Tv::new([9; 32], "Lounge LG", Some("482915"));
    let tv_id = tv.id();
    let stop = Arc::new(AtomicBool::new(false));
    let lobby = {
        let (tv, stop) = (tv.clone(), stop.clone());
        let addr = format!("127.0.0.1:{}", bridge.port());
        std::thread::spawn(move || tv.run(addr, stop))
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
