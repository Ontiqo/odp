//! The receiver for Android (TV): the Java side is `app.omninput.screen.Odp`, whose native methods are these. Events
//! reach the listener's `onEvent(String json, byte[] data)` (the JSON as in `ffi`; `data` is a frame's bytes, else
//! null) on the receiver's own threads.

use crate::{ffi::event_json, Config, Event, Receiver};
use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
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
    match Receiver::start(cfg, move |e| tell(&vm, &listener, &e)) {
        Ok(r) => Box::into_raw(Box::new(r)) as jlong,
        Err(_) => 0,
    }
}

#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_pairing(_: JNIEnv, _: JClass, handle: jlong, open: jboolean) {
    if let Some(r) = receiver(handle) {
        r.pairing(open != 0);
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

#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_decoderFailed(_: JNIEnv, _: JClass, handle: jlong) {
    if let Some(r) = receiver(handle) {
        r.decoder_failed();
    }
}

#[no_mangle]
pub extern "system" fn Java_app_omninput_screen_Odp_latency(_: JNIEnv, _: JClass, handle: jlong, video_us: jlong) {
    if let Some(r) = receiver(handle) {
        r.latency(video_us);
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
