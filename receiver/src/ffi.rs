//! The receiver through a C interface (Apple TV links it as a static library; anything with a C FFI can).
//!
//! Every event comes to one callback as a JSON object (`t` names it: `listening`, `pin`, `paired`, `started`, `frame`,
//! `stopped`, `renamed`, `cinema.open`, `cinema.control`, `cinema.close`, `problem`); a frame's encoded bytes come in
//! `data`/`len` (null and 0 otherwise). The strings and bytes are only valid during the call.

use crate::{Config, Event, Receiver};
use odp::control::Caps;
use std::ffi::{c_char, c_void, CStr, CString};

/// `event` is a JSON object; `data`/`len` a frame's bytes.
pub type Callback = extern "C" fn(ctx: *mut c_void, event: *const c_char, data: *const u8, len: usize);

struct Ctx(*mut c_void);
// The screen app owns the context and says it may be used from the receiver's threads.
unsafe impl Send for Ctx {}
unsafe impl Sync for Ctx {}

/// An event as the JSON the callback gets.
pub fn event_json(e: &Event) -> serde_json::Value {
    use serde_json::json;
    match e {
        Event::Listening { port, fingerprint } => json!({"t": "listening", "port": port, "fingerprint": fingerprint}),
        Event::Pin(pin) => json!({"t": "pin", "pin": pin}),
        Event::Paired { host } => json!({"t": "paired", "host": host}),
        Event::Started { host, codec, w, h } => json!({"t": "started", "host": host, "codec": codec, "w": w, "h": h}),
        Event::Frame { show_at_us, captured_us, key, .. } => json!({"t": "frame", "show_at_us": show_at_us, "captured_us": captured_us, "key": key}),
        Event::Audio { play_at_us, .. } => json!({"t": "audio", "play_at_us": play_at_us}),
        Event::Stopped { host } => json!({"t": "stopped", "host": host}),
        Event::Renamed { name } => json!({"t": "renamed", "name": name}),
        Event::CinemaOpen { service, url, position_ms, rate, playing, at_us } => {
            json!({"t": "cinema.open", "service": service, "url": url, "position_ms": position_ms, "rate": rate, "playing": playing, "at_us": at_us})
        }
        Event::CinemaControl { position_ms, rate, playing, at_us } => {
            json!({"t": "cinema.control", "position_ms": position_ms, "rate": rate, "playing": playing, "at_us": at_us})
        }
        Event::CinemaClose => json!({"t": "cinema.close"}),
        Event::Problem(text) => json!({"t": "problem", "text": text}),
    }
}

unsafe fn text(p: *const c_char) -> Option<String> {
    (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
}

/// Starts a receiver. `dir`: where it keeps its key and paired hosts; `caps_json`: its `hello.caps` (may be null);
/// `kiosk`: nobody sits at it, so a paired host may rename it. Returns null on failure, after a `problem` event
/// that says why.
///
/// # Safety
/// The strings must be valid C strings (or null where allowed); `callback` is called from other threads with `ctx`.
#[no_mangle]
pub unsafe extern "C" fn odp_receiver_start(dir: *const c_char, name: *const c_char, caps_json: *const c_char, kiosk: bool, callback: Callback, ctx: *mut c_void) -> *mut Receiver {
    let (Some(dir), Some(name)) = (text(dir), text(name)) else { return std::ptr::null_mut() };
    let caps: Caps = text(caps_json).and_then(|c| serde_json::from_str(&c).ok()).unwrap_or_default();
    let ctx = Ctx(ctx);
    let cfg = Config { dir: dir.into(), name, port: 0, caps, kiosk, bind: std::net::IpAddr::from([0, 0, 0, 0]) };
    let sink = move |e: Event| {
        let json = CString::new(event_json(&e).to_string()).unwrap_or_default();
        let (data, len) = match &e {
            Event::Frame { data, .. } | Event::Audio { data, .. } => (data.as_ptr(), data.len()),
            _ => (std::ptr::null(), 0),
        };
        let ctx = &ctx;
        callback(ctx.0, json.as_ptr(), data, len);
    };
    let sink = std::sync::Arc::new(sink);
    let tell = sink.clone();
    match Receiver::start(cfg, move |e| sink(e)) {
        Ok(r) => Box::into_raw(Box::new(r)),
        Err(e) => {
            tell(Event::Problem(e));
            std::ptr::null_mut()
        }
    }
}

/// # Safety
/// `r` comes from `odp_receiver_start`.
#[no_mangle]
pub unsafe extern "C" fn odp_receiver_pairing(r: *const Receiver, open: bool) {
    if let Some(r) = r.as_ref() {
        r.pairing(open);
    }
}

/// The sound output changed (`Receiver::output`).
///
/// # Safety
/// `r` comes from `odp_receiver_start`; `device` and `kind` are UTF-8 C strings.
#[no_mangle]
pub unsafe extern "C" fn odp_receiver_output(r: *const Receiver, device: *const std::os::raw::c_char, kind: *const std::os::raw::c_char, reported_us: i64) {
    let text = |p: *const std::os::raw::c_char| if p.is_null() { String::new() } else { std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned() };
    if let Some(r) = r.as_ref() {
        r.output(&text(device), &text(kind), reported_us);
    }
}

/// A meeting room's screen, or not (`Receiver::set_room`).
///
/// # Safety
/// `r` comes from `odp_receiver_start`.
#[no_mangle]
pub unsafe extern "C" fn odp_receiver_room(r: *const Receiver, on: bool) {
    if let Some(r) = r.as_ref() {
        r.set_room(on);
    }
}

/// # Safety
/// `r` comes from `odp_receiver_start`.
#[no_mangle]
pub unsafe extern "C" fn odp_receiver_key(r: *const Receiver, usage: u16, down: bool, modifiers: u8) {
    if let Some(r) = r.as_ref() {
        r.key(usage, down, modifiers);
    }
}

/// # Safety
/// `r` comes from `odp_receiver_start`.
#[no_mangle]
pub unsafe extern "C" fn odp_receiver_pointer(r: *const Receiver, x: u16, y: u16, buttons: u8, wheel: i16) {
    if let Some(r) = r.as_ref() {
        r.pointer(x, y, buttons, wheel);
    }
}

/// # Safety
/// `r` comes from `odp_receiver_start`.
#[no_mangle]
pub unsafe extern "C" fn odp_receiver_decoder_failed(r: *const Receiver) {
    if let Some(r) = r.as_ref() {
        r.decoder_failed();
    }
}

/// # Safety
/// `r` comes from `odp_receiver_start`.
#[no_mangle]
pub unsafe extern "C" fn odp_receiver_latency(r: *const Receiver, video_us: i64) {
    if let Some(r) = r.as_ref() {
        r.latency(video_us);
    }
}

/// `error` may be null (playing or paused fine).
///
/// # Safety
/// `r` comes from `odp_receiver_start`; `error` is a C string or null.
#[no_mangle]
pub unsafe extern "C" fn odp_receiver_cinema_status(r: *const Receiver, position_ms: i64, playing: bool, error: *const c_char) {
    if let Some(r) = r.as_ref() {
        r.cinema_status(position_ms, playing, text(error));
    }
}

/// # Safety
/// `r` comes from `odp_receiver_start`; `id` is a C string.
#[no_mangle]
pub unsafe extern "C" fn odp_receiver_forget(r: *const Receiver, id: *const c_char) {
    if let (Some(r), Some(id)) = (r.as_ref(), text(id)) {
        r.forget(&id);
    }
}

/// `{port, fingerprint, name, hosts: [{id, name}]}`; free it with `odp_receiver_free_string`.
///
/// # Safety
/// `r` comes from `odp_receiver_start`.
#[no_mangle]
pub unsafe extern "C" fn odp_receiver_info(r: *const Receiver) -> *mut c_char {
    let Some(r) = r.as_ref() else { return std::ptr::null_mut() };
    let hosts: Vec<serde_json::Value> = r.hosts().into_iter().map(|(id, name)| serde_json::json!({"id": id, "name": name})).collect();
    let info = serde_json::json!({"port": r.port(), "fingerprint": r.fingerprint(), "name": r.name(), "hosts": hosts});
    CString::new(info.to_string()).map(CString::into_raw).unwrap_or(std::ptr::null_mut())
}

/// # Safety
/// `s` comes from `odp_receiver_info` and is freed once.
#[no_mangle]
pub unsafe extern "C" fn odp_receiver_free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}
