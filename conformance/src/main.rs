//! `odp-check <address:port> <pin> [--key SECONDS] [--switch SECONDS] [--seconds N] [--cinema URL]`: checks a receiver
//! against ODP (1.0 to 1.2) and prints which levels it meets. Show a PIN on the receiver first; `--key` waits for a key
//! pressed at the receiver, `--switch` for the test input chosen in its menu.
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<u64>().ok());
    let (Some(address), Some(pin)) = (args.first(), args.get(1)) else {
        eprintln!("usage: odp-check <address:port> <pin> [--key SECONDS] [--switch SECONDS] [--seconds N] | --show SECONDS [--name NAME] [--video FILE --codec C] | --cinema URL");
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
