//! `odp-check <address:port> <pin> [--key SECONDS] [--seconds N]`: checks a receiver against ODP 0.1 and prints
//! which levels it meets. Show a PIN on the receiver first; `--key` waits for a key pressed at the receiver.
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<u64>().ok());
    let (Some(address), Some(pin)) = (args.first(), args.get(1)) else {
        eprintln!("usage: odp-check <address:port> <pin> [--key SECONDS] [--seconds N]");
        std::process::exit(2);
    };
    let pin: String = pin.chars().filter(char::is_ascii_digit).collect();
    let cfg = odp_check::Config { address: address.clone(), pin, key_wait: Duration::from_secs(flag("--key").unwrap_or(0)), seconds: flag("--seconds").unwrap_or(10) };
    let report = odp_check::run(&cfg);
    print!("{}", report.text());
    std::process::exit(if report.meets(odp_check::Level::Basic) { 0 } else { 1 });
}
