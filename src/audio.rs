//! L4 sound: 48 kHz, 10 ms frames (480 samples per channel), interleaved 16-bit samples, sent on stream 1 either as
//! PCM (the `rt` profile on a LAN) or as Opus (`session.start.audio_codec: "opus"`, when the receiver lists it in
//! `caps.audio_codecs`). Opus here is libopus 1.3.1 (translated to Rust), in its low-delay mode with in-band FEC, so a
//! lost frame is rebuilt from the next one or concealed.

use unsafe_libopus as ffi;

/// Samples a second, per channel.
pub const RATE: u32 = 48_000;
/// Samples in one 10 ms frame, per channel.
pub const FRAME: usize = 480;
/// `session.start.audio_codec` and `caps.audio_codecs` names.
pub const PCM: &str = "pcm";
pub const OPUS: &str = "opus";

/// The largest Opus packet we make for one frame (far above what 10 ms at our bitrates needs).
const MAX_PACKET: usize = 1500;

/// Turns 10 ms frames of interleaved samples into Opus packets.
pub struct Encoder {
    st: *mut ffi::OpusEncoder,
    channels: usize,
}

// The encoder's state belongs to it alone; it is used from one thread at a time.
unsafe impl Send for Encoder {}

impl Encoder {
    /// `channels`: 1 or 2. `bitrate`: bits a second (64 000 a channel is transparent for most listening).
    pub fn new(channels: u8, bitrate: i32) -> Result<Encoder, String> {
        if !(1..=2).contains(&channels) {
            return Err(format!("Opus here takes 1 or 2 channels, not {channels}."));
        }
        let mut err = 0;
        let st = unsafe { ffi::opus_encoder_create(RATE as i32, channels as i32, ffi::OPUS_APPLICATION_RESTRICTED_LOWDELAY, &mut err) };
        if st.is_null() || err != ffi::OPUS_OK {
            return Err(format!("Opus encoder: error {err}"));
        }
        unsafe {
            ffi::opus_encoder_ctl!(st, ffi::OPUS_SET_BITRATE_REQUEST, bitrate);
            // Wi-Fi loses packets: each packet carries enough of the one before to rebuild it.
            ffi::opus_encoder_ctl!(st, ffi::OPUS_SET_INBAND_FEC_REQUEST, 1);
            ffi::opus_encoder_ctl!(st, ffi::OPUS_SET_PACKET_LOSS_PERC_REQUEST, 5);
            ffi::opus_encoder_ctl!(st, ffi::OPUS_SET_COMPLEXITY_REQUEST, 5);
        }
        Ok(Encoder { st, channels: channels as usize })
    }

    /// One 10 ms frame: `FRAME * channels` interleaved samples.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>, String> {
        if pcm.len() != FRAME * self.channels {
            return Err(format!("An Opus frame is {} samples, not {}.", FRAME * self.channels, pcm.len()));
        }
        let mut out = vec![0u8; MAX_PACKET];
        let n = unsafe { ffi::opus_encode(self.st, pcm.as_ptr(), FRAME as i32, out.as_mut_ptr(), out.len() as i32) };
        if n < 0 {
            return Err(format!("Opus encode: error {n}"));
        }
        out.truncate(n as usize);
        Ok(out)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { ffi::opus_encoder_destroy(self.st) }
    }
}

/// Turns Opus packets back into 10 ms frames of interleaved samples.
pub struct Decoder {
    st: *mut ffi::OpusDecoder,
    channels: usize,
}

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(channels: u8) -> Result<Decoder, String> {
        if !(1..=2).contains(&channels) {
            return Err(format!("Opus here takes 1 or 2 channels, not {channels}."));
        }
        let mut err = 0;
        let st = unsafe { ffi::opus_decoder_create(RATE as i32, channels as i32, &mut err) };
        if st.is_null() || err != ffi::OPUS_OK {
            return Err(format!("Opus decoder: error {err}"));
        }
        Ok(Decoder { st, channels: channels as usize })
    }

    fn run(&mut self, packet: Option<&[u8]>, fec: bool) -> Result<Vec<i16>, String> {
        let mut pcm = vec![0i16; FRAME * self.channels];
        let (ptr, len) = packet.map_or((std::ptr::null(), 0), |p| (p.as_ptr(), p.len() as i32));
        let n = unsafe { ffi::opus_decode(self.st, ptr, len, pcm.as_mut_ptr(), FRAME as i32, fec as i32) };
        if n < 0 {
            return Err(format!("Opus decode: error {n}"));
        }
        pcm.truncate(n as usize * self.channels);
        Ok(pcm)
    }

    /// One packet's frame.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<i16>, String> {
        self.run(Some(packet), false)
    }

    /// The frame before `next` that never came, rebuilt from the copy `next` carries (in-band FEC).
    pub fn recover(&mut self, next: &[u8]) -> Result<Vec<i16>, String> {
        self.run(Some(next), true)
    }

    /// A frame that never came and can't be rebuilt: concealed (a smooth continuation, fading out).
    pub fn conceal(&mut self) -> Result<Vec<i16>, String> {
        self.run(None, false)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { ffi::opus_decoder_destroy(self.st) }
    }
}

/// Interleaved samples as stream 1's PCM bytes (little-endian), and back.
pub fn to_bytes(pcm: &[i16]) -> Vec<u8> {
    pcm.iter().flat_map(|s| s.to_le_bytes()).collect()
}

pub fn from_bytes(b: &[u8]) -> Vec<i16> {
    b.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize) -> Vec<i16> {
        (0..frames * FRAME)
            .flat_map(|i| {
                let v = ((i as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin() * 12_000.0) as i16;
                [v, v]
            })
            .collect()
    }

    fn rms(x: &[i16]) -> f64 {
        (x.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
    }

    /// A tone goes through Opus and comes back as the same tone (after the codec's short start), far smaller.
    #[test]
    fn a_tone_survives_opus() {
        let pcm = tone(50);
        let mut enc = Encoder::new(2, 128_000).unwrap();
        let mut dec = Decoder::new(2).unwrap();
        let mut out = Vec::new();
        let mut bytes = 0;
        for f in pcm.chunks(FRAME * 2) {
            let p = enc.encode(f).unwrap();
            bytes += p.len();
            out.extend(dec.decode(&p).unwrap());
        }
        assert_eq!(out.len(), pcm.len());
        assert!(bytes < pcm.len() * 2 / 6, "{bytes} bytes for {} of PCM", pcm.len() * 2);
        // Compare the second half, allowing for the codec's few-millisecond delay: find the best shift.
        let half = pcm.len() / 2;
        let best = (0..400)
            .step_by(2)
            .map(|d| {
                let a = &pcm[half..half + 9600];
                let b = &out[half + d..half + d + 9600];
                rms(&a.iter().zip(b).map(|(x, y)| x.saturating_sub(*y)).collect::<Vec<_>>())
            })
            .fold(f64::MAX, f64::min);
        assert!(best < rms(&pcm[half..]) * 0.2, "error {best} against signal {}", rms(&pcm[half..]));
    }

    /// A lost frame is rebuilt from the next packet's FEC, or concealed: either way a full frame of sound.
    #[test]
    fn a_lost_frame_is_rebuilt_or_concealed() {
        let pcm = tone(20);
        let mut enc = Encoder::new(2, 96_000).unwrap();
        let packets: Vec<Vec<u8>> = pcm.chunks(FRAME * 2).map(|f| enc.encode(f).unwrap()).collect();
        let mut dec = Decoder::new(2).unwrap();
        for p in &packets[..10] {
            dec.decode(p).unwrap();
        }
        let rebuilt = dec.recover(&packets[11]).unwrap();
        assert_eq!(rebuilt.len(), FRAME * 2);
        assert!(rms(&rebuilt) > 1000.0, "rebuilt frame is quiet: {}", rms(&rebuilt));
        dec.decode(&packets[11]).unwrap();
        let concealed = dec.conceal().unwrap();
        assert_eq!(concealed.len(), FRAME * 2);
    }

    #[test]
    fn pcm_bytes_round_trip() {
        let x = vec![0i16, 1, -1, i16::MAX, i16::MIN];
        assert_eq!(from_bytes(&to_bytes(&x)), x);
    }
}
