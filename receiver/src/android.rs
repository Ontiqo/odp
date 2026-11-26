//! The receiver for Android (TV): the Java side is `app.omninput.screen.Odp`, whose native methods are these. Events
//! reach the listener's `onEvent(String json, byte[] data)` (the JSON as in `ffi`; `data` is a frame's bytes, else
//! null) on the receiver's own threads.

use crate::{ffi::event_json, Config, Event, Receiver};
use jni::objects::{GlobalRef, JClass, JIntArray, JObject, JString, JValue};
use jni::sys::{jboolean, jint, jlong, jshort, jstring};
use jni::{JNIEnv, JavaVM};

fn receiver<'a>(handle: jlong) -> Option<&'a Receiver> {
    // SAFETY: the handle is the pointer `start` returned; the app never frees it (the receiver lives as long as the
    // process).
    unsafe { (handle as *const Receiver).as_ref() }
}

fn tell(vm: &JavaVM, listener: &GlobalRef, e: &Event) {
    let Ok(mut env) = vm.attach_current_thread_permanently() else { return };
    // Each call's references are freed with its frame (these threads stay attached).
    let _ = env.with_local_frame(8, |env| -> jni::errors::Result<()> {
        let json = env.new_string(event_json(e).to_string())?;
        let data = match e {
            Event::Frame { data, .. } | Event::Audio { data, .. } => JObject::from(env.byte_array_from_slice(data)?),
            _ => JObject::null(),
        };
        env.call_method(listener.as_obj(), "onEvent", "(Ljava/lang/String;[B)V", &[JValue::Object(&json), JValue::Object(&data)])?;
        Ok(())
    });
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
}

#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_start(mut env: JNIEnv, _: JClass, dir: JString, name: JString, caps: JString, kiosk: jboolean, listener: JObject) -> jlong {
    let (Ok(dir), Ok(name)) = (env.get_string(&dir).map(String::from), env.get_string(&name).map(String::from)) else { return 0 };
    let caps = env.get_string(&caps).map(String::from).ok().and_then(|c| serde_json::from_str(&c).ok()).unwrap_or_default();
    let (Ok(vm), Ok(listener)) = (env.get_java_vm(), env.new_global_ref(listener)) else { return 0 };
    let cfg = Config { dir: dir.into(), name, port: 0, caps, kiosk: kiosk != 0, bind: std::net::IpAddr::from([0, 0, 0, 0]) };
    let sink = std::sync::Arc::new(move |e: Event| tell(&vm, &listener, &e));
    let say = sink.clone();
    match Receiver::start(cfg, move |e| sink(e)) {
        Ok(r) => Box::into_raw(Box::new(r)) as jlong,
        // The reason, as a `problem` event, before the app hears 0.
        Err(e) => {
            say(Event::Problem(e));
            0
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_pairing(_: JNIEnv, _: JClass, handle: jlong, open: jboolean) {
    if let Some(r) = receiver(handle) {
        r.pairing(open != 0);
    }
}

/// The sound output changed (`Receiver::output`).
#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_output(mut env: JNIEnv, _: JClass, handle: jlong, device: JString, kind: JString, reported_us: jlong) {
    let device: String = env.get_string(&device).map(Into::into).unwrap_or_default();
    let kind: String = env.get_string(&kind).map(Into::into).unwrap_or_default();
    if let Some(r) = receiver(handle) {
        r.output(&device, &kind, reported_us);
    }
}

/// Names this screen on itself (`Receiver::rename`); empty goes back to the device's own name.
#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_rename(mut env: JNIEnv, _: JClass, handle: jlong, name: JString) {
    let name: String = env.get_string(&name).map(Into::into).unwrap_or_default();
    if let Some(r) = receiver(handle) {
        r.rename(&name);
    }
}

/// A meeting room's screen, or not (`Receiver::set_room`).
#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_room(_: JNIEnv, _: JClass, handle: jlong, on: jboolean) {
    if let Some(r) = receiver(handle) {
        r.set_room(on != 0);
    }
}

#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_key(_: JNIEnv, _: JClass, handle: jlong, usage: jint, down: jboolean, modifiers: jint) {
    if let Some(r) = receiver(handle) {
        r.key(usage as u16, down != 0, modifiers as u8);
    }
}

#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_pointer(_: JNIEnv, _: JClass, handle: jlong, x: jint, y: jint, buttons: jint, wheel: jshort) {
    if let Some(r) = receiver(handle) {
        r.pointer(x as u16, y as u16, buttons as u8, wheel);
    }
}

/// Touch: the fingers down now (and any just lifted), as parallel arrays; states 0 down, 1 move, 2 up.
#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_touch(env: JNIEnv, _: JClass, handle: jlong, ids: JIntArray, xs: JIntArray, ys: JIntArray, states: JIntArray) {
    use odp::input::{Contact, TouchState};
    let read = |a: &JIntArray| -> Vec<i32> {
        let n = env.get_array_length(a).unwrap_or(0).max(0) as usize;
        let mut v = vec![0i32; n];
        let _ = env.get_int_array_region(a, 0, &mut v);
        v
    };
    let (ids, xs, ys, states) = (read(&ids), read(&xs), read(&ys), read(&states));
    let n = ids.len().min(xs.len()).min(ys.len()).min(states.len()).min(255);
    let contacts = (0..n)
        .map(|i| Contact { id: ids[i] as u8, x: xs[i].clamp(0, 65535) as u16, y: ys[i].clamp(0, 65535) as u16, state: match states[i] { 0 => TouchState::Down, 1 => TouchState::Move, _ => TouchState::Up } })
        .collect();
    if let Some(r) = receiver(handle) {
        r.touch(contacts);
    }
}

/// A pen (stylus): where and how hard (0..65535 each), its flags (1 in range, 2 tip, 4 barrel, 8 eraser).
#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_pen(_: JNIEnv, _: JClass, handle: jlong, x: jint, y: jint, pressure: jint, flags: jint) {
    if let Some(r) = receiver(handle) {
        r.pen(x.clamp(0, 65535) as u16, y.clamp(0, 65535) as u16, pressure.clamp(0, 65535) as u16, flags as u8);
    }
}

/// Whether the focused host takes game controllers as controllers (send them with `pad`, else as keys).
#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_controllersTaken(_: JNIEnv, _: JClass, handle: jlong) -> jboolean {
    receiver(handle).is_some_and(|r| r.controllers_taken()) as jboolean
}

/// A game controller's whole state (ODP 1.1): which (0..3), buttons (odp::input::pad bits), triggers 0..255, sticks
/// -32768..32767 with up positive.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_app_omninput_screen_Odp_pad(_: JNIEnv, _: JClass, handle: jlong, index: jint, buttons: jint, lt: jint, rt: jint, lx: jint, ly: jint, rx: jint, ry: jint) {
    if let Some(r) = receiver(handle) {
        let axis = |v: jint| v.clamp(-32768, 32767) as i16;
        r.pad(odp::input::Pad { index: index.clamp(0, 3) as u8, buttons: buttons as u16, lt: lt.clamp(0, 255) as u8, rt: rt.clamp(0, 255) as u8, lx: axis(lx), ly: axis(ly), rx: axis(rx), ry: axis(ry) });
    }
}

#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_decoderFailed(_: JNIEnv, _: JClass, handle: jlong, slot: jint) {
    if let Some(r) = receiver(handle) {
        r.decoder_failed(slot as u32);
    }
}

/// Ends a host's link (`Receiver::end`).
#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_end(_: JNIEnv, _: JClass, handle: jlong, slot: jint) {
    if let Some(r) = receiver(handle) {
        r.end(slot as u32);
    }
}

/// Which slot keys and the pointer go to (`Receiver::focus`).
#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_focus(_: JNIEnv, _: JClass, handle: jlong, slot: jint) {
    if let Some(r) = receiver(handle) {
        r.focus(slot as u32);
    }
}

#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_latency(_: JNIEnv, _: JClass, handle: jlong, slot: jint, video_us: jlong) {
    if let Some(r) = receiver(handle) {
        r.latency(slot as u32, video_us);
    }
}

#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_cinemaStatus(mut env: JNIEnv, _: JClass, handle: jlong, position_ms: jlong, playing: jboolean, error: JString) {
    let error = (!error.is_null()).then(|| env.get_string(&error).map(String::from).ok()).flatten();
    if let Some(r) = receiver(handle) {
        r.cinema_status(position_ms, playing != 0, error);
    }
}

#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_forget(mut env: JNIEnv, _: JClass, handle: jlong, id: JString) {
    let Ok(id) = env.get_string(&id).map(String::from) else { return };
    if let Some(r) = receiver(handle) {
        r.forget(&id);
    }
}

/// `{port, fingerprint, name, hosts: [{id, name}]}`.
#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_info(env: JNIEnv, _: JClass, handle: jlong) -> jstring {
    let Some(r) = receiver(handle) else { return std::ptr::null_mut() };
    let hosts: Vec<serde_json::Value> = r.hosts().into_iter().map(|(id, name)| serde_json::json!({"id": id, "name": name})).collect();
    let info = serde_json::json!({"port": r.port(), "fingerprint": r.fingerprint(), "name": r.name(), "hosts": hosts});
    env.new_string(info.to_string()).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}
