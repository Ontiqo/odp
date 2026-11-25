//! The receiver for JavaScript, as plain exports over the module's memory (no generated glue): the page writes
//! input with `odp_alloc`, calls a function, then reads the output buffer `odp_out` gives (`odp_out_len` bytes):
//! records of kind (u8: 0 control bytes, 1 media datagram, 2 event JSON, 3 frame, 4 sound), length (u32 LE), and
//! for a frame show_at_us (i64 LE) and key (u8), for sound play_at_us (i64 LE), before the data. `web/page/receiver.js` is that glue.

use crate::{Out, Web};
use std::cell::RefCell;

// Randomness from the page: `env.odp_random(ptr, len)` fills memory with crypto.getRandomValues.
#[cfg(target_arch = "wasm32")]
mod random {
    #[link(wasm_import_module = "env")]
    extern "C" {
        fn odp_random(ptr: *mut u8, len: usize);
    }
    fn fill(buf: &mut [u8]) -> Result<(), getrandom::Error> {
        // SAFETY: the page writes exactly `len` bytes at `ptr`.
        unsafe { odp_random(buf.as_mut_ptr(), buf.len()) };
        Ok(())
    }
    getrandom::register_custom_getrandom!(fill);
}

thread_local! {
    static WEB: RefCell<Option<Web>> = const { RefCell::new(None) };
    static OUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        return &[];
    }
    // SAFETY: the page passes memory it got from odp_alloc, of that length.
    unsafe { std::slice::from_raw_parts(ptr, len) }
}

fn with(f: impl FnOnce(&mut Web)) {
    WEB.with(|w| {
        if let Some(web) = w.borrow_mut().as_mut() {
            f(web);
            let out = web.take();
            OUT.with(|o| {
                let mut o = o.borrow_mut();
                for item in out {
                    match item {
                        Out::Control(b) => record(&mut o, 0, &[], &b),
                        Out::Media(b) => record(&mut o, 1, &[], &b),
                        Out::Event(e) => record(&mut o, 2, &[], e.as_bytes()),
                        Out::Audio { data, play_at_us } => record(&mut o, 4, &play_at_us.to_le_bytes(), &data),
                        Out::Frame { data, show_at_us, key } => {
                            let mut head = show_at_us.to_le_bytes().to_vec();
                            head.push(key as u8);
                            record(&mut o, 3, &head, &data)
                        }
                    }
                }
            });
        }
    });
}

fn record(o: &mut Vec<u8>, kind: u8, head: &[u8], data: &[u8]) {
    o.push(kind);
    o.extend_from_slice(&((head.len() + data.len()) as u32).to_le_bytes());
    o.extend_from_slice(head);
    o.extend_from_slice(data);
}

#[no_mangle]
pub extern "C" fn odp_alloc(len: usize) -> *mut u8 {
    let mut v = vec![0u8; len.max(1)];
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// # Safety
/// `ptr`/`len` come from `odp_alloc`.
#[no_mangle]
pub unsafe extern "C" fn odp_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        drop(Vec::from_raw_parts(ptr, len.max(1), len.max(1)));
    }
}

/// Starts the receiver: `name` (UTF-8), `caps` (hello.caps JSON), `secret` (32 bytes kept from before, or len 0),
/// `known` (JSON `[[id, key base64], ...]` of paired hosts), `udp_port` (the bridge's media port).
#[no_mangle]
pub extern "C" fn odp_start(name: *const u8, name_len: usize, caps: *const u8, caps_len: usize, secret: *const u8, secret_len: usize, known: *const u8, known_len: usize, udp_port: u32) {
    let name = String::from_utf8_lossy(bytes(name, name_len)).into_owned();
    let caps = serde_json::from_slice(bytes(caps, caps_len)).unwrap_or_default();
    let secret = <[u8; 32]>::try_from(bytes(secret, secret_len)).ok();
    let mut web = Web::new(&name, caps, secret);
    web.udp_port = udp_port as u16;
    if let Ok(serde_json::Value::Array(list)) = serde_json::from_slice(bytes(known, known_len)) {
        for item in list {
            let (Some(id), Some(key)) = (item[0].as_str(), item[1].as_str().and_then(odp::link::unb64)) else { continue };
            if let Ok(key) = <[u8; 32]>::try_from(key.as_slice()) {
                web.know(id, key);
            }
        }
    }
    let info = serde_json::json!({"t": "started_receiver", "id": web.id(), "fingerprint": web.fingerprint(), "secret": odp::link::b64(&web.secret())});
    WEB.with(|w| *w.borrow_mut() = Some(web));
    OUT.with(|o| record(&mut o.borrow_mut(), 2, &[], info.to_string().as_bytes()));
}

/// A new WebSocket to the casting computer: the handshake starts again.
#[no_mangle]
pub extern "C" fn odp_connected() {
    with(|w| w.connected());
}

#[no_mangle]
pub extern "C" fn odp_control(ptr: *const u8, len: usize, now_ms: f64) {
    let b = bytes(ptr, len).to_vec();
    with(|w| w.control(&b, (now_ms * 1000.0) as i64));
}

#[no_mangle]
pub extern "C" fn odp_media(ptr: *const u8, len: usize, now_ms: f64) {
    let b = bytes(ptr, len).to_vec();
    with(|w| w.media(&b, (now_ms * 1000.0) as i64));
}

#[no_mangle]
pub extern "C" fn odp_ping(now_ms: f64) {
    with(|w| w.ping((now_ms * 1000.0) as i64));
}

#[no_mangle]
pub extern "C" fn odp_pairing(open: u32) {
    with(|w| w.pairing(open != 0));
}

#[no_mangle]
pub extern "C" fn odp_key(usage: u32, down: u32, modifiers: u32) {
    with(|w| w.key(usage as u16, down != 0, modifiers as u8));
}

#[no_mangle]
pub extern "C" fn odp_latency(video_us: f64) {
    with(|w| w.latency(video_us as i64));
}

#[no_mangle]
pub extern "C" fn odp_decoder_failed() {
    with(|w| w.decoder_failed());
}

/// The output since the last call (records as above); valid until the next call into the module.
#[no_mangle]
pub extern "C" fn odp_out() -> *const u8 {
    OUT.with(|o| {
        let mut o = o.borrow_mut();
        let taken = std::mem::take(&mut *o);
        LAST.with(|l| {
            *l.borrow_mut() = taken;
            l.borrow().as_ptr()
        })
    })
}

#[no_mangle]
pub extern "C" fn odp_out_len() -> usize {
    LAST.with(|l| l.borrow().len())
}

thread_local! {
    static LAST: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}
