//! L1 control channel and its messages (L3 session, L5 clock and sync, L6 reliable keys, L7 cinema).
//!
//! Framing: `u32` big-endian length, then the message. Before the session keys exist (`hello`, `pair.*`,
//! `session.offer`), the message is JSON as it is; after, it is the JSON sealed on the control channel. JSON keeps the
//! protocol easy to read and to implement anywhere; every message has `t` (its type) and requests have `id`, which the
//! reply repeats.

use serde::{Deserialize, Serialize};

/// The longest framed message accepted (64 KiB): control messages are small; media goes over UDP.
pub const MAX_MESSAGE: usize = 64 * 1024;

/// A video format a receiver can decode.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Decode {
    /// "h264", "hevc" or "av1".
    pub codec: String,
    pub max_w: u32,
    pub max_h: u32,
    pub max_fps: u32,
    /// In hardware (cheap to run) or not.
    pub hw: bool,
}

/// What a receiver can do (`hello.caps`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Caps {
    #[serde(default)]
    pub decode: Vec<Decode>,
    /// Pictures it plays at once (one per computer shown, side by side; 0: not said, take it as 1). A screen that plays
    /// fewer than the computers that want to show gets them combined into one picture by one of them (ODP 1.5).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub streams: u8,
    /// The display: pixels and the refresh rates it can switch to.
    #[serde(default)]
    pub display: Option<Display>,
    /// Audio channels it plays (0: no sound).
    #[serde(default)]
    pub audio_channels: u8,
    /// Sound formats it takes besides PCM ("opus"); empty: PCM only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio_codecs: Vec<String>,
    #[serde(default)]
    pub keyboard: bool,
    #[serde(default)]
    pub pointer: bool,
    /// Touch contacts it reports at once (0: no touch).
    #[serde(default)]
    pub touch: u8,
    /// It sends a pen's position, pressure and buttons (input kind 3).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pen: bool,
    /// Game controllers it sends (input kind 4, ODP 1.1): how many at once (0: none).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub controllers: u8,
    /// It takes `room.watch` and `room.end` from the computers that own it (ODP 1.1): moderating who shows.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub room: bool,
    /// It lists the other inputs of the monitor or TV it is on and asks for them (`monitor.*`, ODP 1.2): a games
    /// console's screen app switching the monitor to a computer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inputs: bool,
    /// DRM systems its film player has ("widevine", "playready", "fairplay"); empty: no cinema.
    #[serde(default)]
    pub cinema: Vec<String>,
}

/// Another input of the monitor a receiver is on (`monitor.inputs`): what the host calls it (`id`, opaque to the
/// receiver) and its name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInput {
    pub id: String,
    pub name: String,
}

/// A computer showing on a receiver (`room.presenters`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presenter {
    pub slot: u32,
    pub name: String,
    /// The host's id from its hello (ODP 1.5, in `screen.showing`): how computers recognise each other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Display {
    pub w: u32,
    pub h: u32,
    pub refresh: Vec<u32>,
    /// Latencies from the display's EDID (HDMI), when it states them.
    #[serde(default)]
    pub video_latency_ms: Option<u32>,
    #[serde(default)]
    pub audio_latency_ms: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Mirror,
    Extend,
    Cinema,
}

/// A part of the picture, each edge as a fraction of its width or height (0..=65535): a screen of a wall shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlayState {
    Playing,
    Paused,
}

/// Every control message. `t` names the variant on the wire (`"session.start"`, …).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Message {
    #[serde(rename = "hello")]
    Hello { versions: Vec<String>, id: String, name: String, role: String, #[serde(default)] caps: Caps },
    #[serde(rename = "pair.start")]
    PairStart { spake: String },
    #[serde(rename = "pair.confirm")]
    PairConfirm { sealed: String },
    #[serde(rename = "session.offer")]
    SessionOffer { ephemeral: String, identity: String, signature: String },
    #[serde(rename = "session.start")]
    SessionStart {
        id: u32,
        mode: Mode,
        codec: String,
        w: u32,
        h: u32,
        fps: u32,
        bitrate_kbps: u32,
        audio: bool,
        /// The sound's format on stream 1: "opus", or "pcm" (absent: pcm).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio_codec: Option<String>,
        group: Option<String>,
        /// A wall: the part of the picture this receiver shows, enlarged to fill its screen (absent: all of it).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<Region>,
    },
    #[serde(rename = "session.ok")]
    SessionOk { id: u32, udp_port: u16 },
    #[serde(rename = "session.update")]
    SessionUpdate { fps: Option<u32>, bitrate_kbps: Option<u32>, w: Option<u32>, h: Option<u32> },
    #[serde(rename = "session.keyframe")]
    Keyframe,
    #[serde(rename = "session.stop")]
    SessionStop,
    /// A paired host names a device that has nobody at it (a kiosk); a device with its own owner answers with an error.
    #[serde(rename = "device.rename")]
    Rename { name: String },
    #[serde(rename = "media.nack")]
    Nack { stream: u8, seqs: Vec<u32> },
    #[serde(rename = "media.report")]
    Report { lost: u32, received: u32, jitter_us: u32 },
    #[serde(rename = "sync.target")]
    SyncTarget { delay_us: i64 },
    #[serde(rename = "sync.latency")]
    SyncLatency { video_us: i64, audio_us: i64, source: String },
    #[serde(rename = "sync.output")]
    SyncOutput { device: String, kind: String, reported_us: i64 },
    #[serde(rename = "sync.offset")]
    SyncOffset { us: i64 },
    #[serde(rename = "cal.chirp")]
    Chirp { at_us: i64, signal: String, gain: f32 },
    #[serde(rename = "cal.result")]
    CalResult { receiver: String, arrival_us: i64, level_db: f32 },
    #[serde(rename = "input.key")]
    Key { usage: u16, down: bool, modifiers: u8 },
    /// ODP 1.1, host to receiver, only to receivers whose `caps.controllers` isn't 0: whether this host takes their game
    /// controllers as controllers (`on`), so the receiver sends them as such and not as keys.
    #[serde(rename = "input.controllers")]
    Controllers { on: bool },
    /// ODP 1.1, host to receiver, only to receivers whose caps say `room`: this host (one paired with the screen
    /// outside meeting-room mode: an owner) wants `room.presenters` while `on`.
    #[serde(rename = "room.watch")]
    RoomWatch { on: bool },
    /// ODP 1.1, receiver to an owner watching: the computers showing here now, each by its slot and name.
    #[serde(rename = "room.presenters")]
    RoomPresenters { list: Vec<Presenter> },
    /// ODP 1.1, an owner to the receiver: end that presenter's link (as the screen's own "end" does).
    #[serde(rename = "room.end")]
    RoomEnd { slot: u32 },
    /// ODP 1.5, receiver to every host that speaks 1.5, whenever it changes: the computers showing here now, in the
    /// order they started, with their ids. With `caps.streams` they agree among themselves who combines
    /// (Omninput's docs/COMBINING.md).
    #[serde(rename = "screen.showing")]
    ScreenShowing { list: Vec<Presenter> },
    /// ODP 1.2, an owner to a receiver with `caps.inputs`: the other inputs of the monitor it is on (computers, and
    /// what else is plugged in), sent when the link starts and whenever they change.
    #[serde(rename = "monitor.inputs")]
    MonitorInputs { list: Vec<MonitorInput> },
    /// ODP 1.2, the receiver to one owner that listed inputs: switch the monitor to that one.
    #[serde(rename = "monitor.switch")]
    MonitorSwitch { id: String },
    /// ODP 1.3, the owner to the receiver that asked, once it has acted on `monitor.switch`: `ok` when the monitor
    /// switched (or the computers that can switch it were asked); otherwise `text` says why not, in plain words for
    /// the screen to show. Sent only to receivers whose `hello` lists `odp/1.3`.
    #[serde(rename = "monitor.switched")]
    MonitorSwitched { id: String, ok: bool, #[serde(default)] text: String },
    #[serde(rename = "cinema.open")]
    CinemaOpen { service: String, url: String, position_ms: i64, rate: f64, state: PlayState, at_us: i64 },
    #[serde(rename = "cinema.control")]
    CinemaControl { state: PlayState, position_ms: i64, rate: f64, at_us: i64 },
    #[serde(rename = "cinema.status")]
    CinemaStatus { position_ms: i64, at_us: i64, state: PlayState, error: Option<String> },
    #[serde(rename = "cinema.close")]
    CinemaClose,
    #[serde(rename = "error")]
    Error { id: Option<u32>, code: String, text: String },
    #[serde(rename = "bye")]
    Bye,
}

impl Message {
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("messages always serialise")
    }

    /// Parses a message; unknown fields are ignored (minor versions add them), unknown types are an error the
    /// caller may ignore.
    pub fn from_json(bytes: &[u8]) -> Result<Message, String> {
        serde_json::from_slice(bytes).map_err(|e| e.to_string())
    }
}

/// `u32` big-endian length, then `body`.
pub fn frame(body: &[u8]) -> Vec<u8> {
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(body);
    out
}

/// Reads one frame from a stream; Ok(None) at a clean end. A length over [`MAX_MESSAGE`] is an error, before reading.
pub fn read_frame(r: &mut impl std::io::Read) -> std::io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_be_bytes(len) as usize;
    if n == 0 || n > MAX_MESSAGE {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "a control message of a wrong size"));
    }
    let mut body = vec![0u8; n];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_their_type_and_round_trip() {
        let m = Message::SessionStart { id: 1, mode: Mode::Mirror, codec: "h264".into(), w: 1920, h: 1080, fps: 60, bitrate_kbps: 20_000, audio: true, audio_codec: Some("opus".into()), group: None, region: None };
        let json = String::from_utf8(m.to_json()).unwrap();
        assert!(json.starts_with(r#"{"t":"session.start","id":1,"mode":"mirror""#), "{json}");
        assert!(!json.contains("region"), "{json}");
        assert_eq!(Message::from_json(json.as_bytes()).unwrap(), m);
        // A wall's screen: the top-right quarter.
        let wall = Message::SessionStart { id: 2, mode: Mode::Mirror, codec: "h264".into(), w: 0, h: 0, fps: 30, bitrate_kbps: 12_000, audio: false, audio_codec: None, group: Some("lobby".into()), region: Some(Region { x: 32768, y: 0, w: 32767, h: 32768 }) };
        assert_eq!(Message::from_json(&wall.to_json()).unwrap(), wall);
    }

    #[test]
    fn unknown_fields_are_ignored_and_unknown_types_are_errors() {
        let m = Message::from_json(br#"{"t":"sync.offset","us":-40000,"later":true}"#).unwrap();
        assert_eq!(m, Message::SyncOffset { us: -40_000 });
        let e = Message::from_json(br#"{"t":"something.new"}"#).unwrap_err();
        // Link::recv passes over exactly these (ODP 1.5).
        assert!(e.contains("unknown variant"), "{e}");
    }

    #[test]
    fn a_hello_without_caps_still_reads() {
        let m = Message::from_json(br#"{"t":"hello","versions":["odp/0.1"],"id":"r1","name":"Projector","role":"receiver"}"#).unwrap();
        assert!(matches!(m, Message::Hello { caps, .. } if caps == Caps::default()));
    }

    #[test]
    fn frames_round_trip_and_oversized_ones_are_refused() {
        let body = Message::Bye.to_json();
        let f = frame(&body);
        assert_eq!(read_frame(&mut &f[..]).unwrap(), Some(body));
        assert_eq!(read_frame(&mut &[][..]).unwrap(), None);
        let big = ((MAX_MESSAGE + 1) as u32).to_be_bytes();
        assert!(read_frame(&mut &big[..]).is_err());
    }

    #[test]
    fn a_rename_round_trips() {
        let m = Message::Rename { name: "Living room TV".into() };
        let j = m.to_json();
        assert!(String::from_utf8_lossy(&j).contains("\"t\":\"device.rename\""));
        assert_eq!(Message::from_json(&j).unwrap(), m);
    }
}

/// For `skip_serializing_if`: a zero count is left out.
fn is_zero(n: &u8) -> bool {
    *n == 0
}
