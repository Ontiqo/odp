//! The conformance host against the reference receiver, with a stand-in screen app around it: it reports a delay
//! once frames arrive and answers Cinema.
use odp_receiver::{Config, Event, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[test]
fn the_reference_receiver_meets_basic_synced_and_cinema() {
    let dir = std::env::temp_dir().join(format!("odp-check-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let caps = odp::control::Caps { decode: vec![odp::control::Decode { codec: "h264".into(), max_w: 1920, max_h: 1080, max_fps: 60, hw: false }], keyboard: true, cinema: vec!["widevine".into()], ..Default::default() };
    let pin = Arc::new(Mutex::new(None::<String>));
    let me: Arc<Mutex<Option<Receiver>>> = Arc::new(Mutex::new(None));
    let (p, m) = (pin.clone(), me.clone());
    let frames = Arc::new(Mutex::new(0u32));
    let r = Receiver::start(Config { dir: dir.clone(), name: "Reference".into(), port: 0, caps, kiosk: false, bind: [127, 0, 0, 1].into() }, move |e| match e {
        Event::Pin(x) => *p.lock().unwrap() = x,
        Event::Frame { .. } => {
            let mut n = frames.lock().unwrap();
            *n += 1;
            if *n == 20 {
                if let Some(r) = m.lock().unwrap().as_ref() {
                    r.latency(40_000);
                }
            }
        }
        Event::CinemaOpen { .. } => {
            if let Some(r) = m.lock().unwrap().as_ref() {
                r.cinema_status(0, false, Some("unavailable".into()));
            }
        }
        _ => {}
    })
    .unwrap();
    *me.lock().unwrap() = Some(r.clone());
    r.pairing(true);
    let pin = loop {
        if let Some(p) = pin.lock().unwrap().clone() {
            break p;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let report = odp_check::run(&odp_check::Config { address: format!("127.0.0.1:{}", r.port()), pin, key_wait: Duration::ZERO, seconds: 8 });
    print!("{}", report.text());
    assert!(report.meets(odp_check::Level::Basic), "{}", report.text());
    assert!(report.meets(odp_check::Level::Synced), "{}", report.text());
    assert!(report.meets(odp_check::Level::Cinema), "{}", report.text());
    let _ = std::fs::remove_dir_all(&dir);
}
