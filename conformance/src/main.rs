//! `odp-check <address:port> <pin> [--key SECONDS] [--switch SECONDS] [--seconds N] [--cinema URL]`: checks a receiver
//! against ODP (1.0 to 1.2) and prints which levels it meets. Show a PIN on the receiver first; `--key` waits for a key
//! pressed at the receiver, `--switch` for the test input chosen in its menu.
use std::time::Duration;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |args: &[String], name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<u64>().ok());
    // --out <pin> [--port P] [--lan]: a screen that connects out (SPEC 1.4) dials this host; the checks then run
    // against it through the bridge. Listens on 127.0.0.1 unless --lan (every interface: a TV on the network).
    let _bridge;
    if args.first().map(String::as_str) == Some("--out") {
        let pin = args.get(1).cloned().unwrap_or_default();
        let lan = args.iter().any(|a| a == "--lan");
        let port = flag(&args, "--port").unwrap_or(odp_out::PORT as u64) as u16;
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let bind: std::net::IpAddr = if lan { [0, 0, 0, 0].into() } else { [127, 0, 0, 1].into() };
        let bridge = match odp_out::Bridge::start(bind, port, "ODP conformance host", move |e| {
            let _ = tx.lock().unwrap().send(e);
        }) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("can't listen for screens: {e}");
                std::process::exit(2);
            }
        };
        eprintln!("Waiting for a screen to connect out to port {} (up to 60 s).", bridge.port());
        let screen = loop {
            match rx.recv_timeout(Duration::from_secs(60)) {
                Ok(odp_out::Event::Screen(s)) => break s,
                Ok(_) => continue,
                Err(_) => {
                    eprintln!("no screen connected");
                    std::process::exit(1);
                }
            }
        };
        eprintln!("{} connected out ({})", screen.name, screen.address);
        args = [vec![screen.address.to_string(), pin], args[2..].to_vec()].concat();
        _bridge = Some(bridge);
    } else {
        _bridge = None;
    }
    let flag = |name: &str| flag(&args, name);
    let (Some(address), Some(pin)) = (args.first(), args.get(1)) else {
        eprintln!("usage: odp-check <address:port> <pin> | --out <pin> [--lan] [--port P], then [--key SECONDS] [--switch SECONDS] [--seconds N] | --show SECONDS [--name NAME] [--video FILE --codec C] | --cinema URL");
        std::process::exit(2);
    };
    let pin: String = pin.chars().filter(char::is_ascii_digit).collect();
    let cfg = odp_check::Config { address: address.clone(), pin, key_wait: Duration::from_secs(flag("--key").unwrap_or(0)), seconds: flag("--seconds").unwrap_or(10), switch_wait: Duration::from_secs(flag("--switch").unwrap_or(0)) };
    // --show SECONDS [--name NAME] [--video FILE --codec C]: the test video on the receiver for that long instead of the checks.
    if let Some(seconds) = flag("--show") {
        let name = args.iter().position(|a| a == "--name").and_then(|i| args.get(i + 1)).map_or("ODP test host", String::as_str);
        // --video FILE --codec hevc: frames from a file (key flag, u32 length, access unit) instead of the test video.
        let video = match args.iter().position(|a| a == "--video").and_then(|i| args.get(i + 1)) {
            Some(path) => {
                let codec = args.iter().position(|a| a == "--codec").and_then(|i| args.get(i + 1)).map_or("h264", String::as_str);
                match odp_check::Video::read(path, codec) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(2);
                    }
                }
            }
            None => None,
        };
        if let Err(e) = odp_check::show_video(&cfg, name, seconds, video) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
    // --cinema URL: follow a real title on the receiver instead of the checks.
    if let Some(url) = args.iter().position(|a| a == "--cinema").and_then(|i| args.get(i + 1)) {
        match odp_check::cinema_follow(&cfg, url) {
            Ok(s) => std::process::exit(if s.is_empty() { 1 } else { 0 }),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }
    let report = odp_check::run(&cfg);
    print!("{}", report.text());
    std::process::exit(if report.meets(odp_check::Level::Basic) { 0 } else { 1 });
}
