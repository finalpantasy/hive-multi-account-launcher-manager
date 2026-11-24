//! A soft chime for alerts: two gentle notes a fifth apart that fade out in
//! under a second. Made in code (no sound file to ship) and played with
//! Windows' PlaySound. Silent while Windows says not to disturb: Do not
//! disturb, a presentation, or a full-screen game or video.

use std::sync::OnceLock;

use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
use windows_sys::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_ACCEPTS_NOTIFICATIONS};

/// The chime as a WAV file in memory (made once, kept for PlaySound's async play).
static WAV: OnceLock<Vec<u8>> = OnceLock::new();

const RATE: u32 = 44_100;

fn chime_wav() -> Vec<u8> {
    let len = (RATE as f32 * 0.9) as usize;
    let mut s = vec![0f32; len];
    // E5, then B5 a moment later: a soft rising fifth.
    for (start, freq) in [(0.0f32, 659.25f32), (0.11, 987.77)] {
        let s0 = (start * RATE as f32) as usize;
        for (i, v) in s.iter_mut().enumerate().skip(s0) {
            let t = (i - s0) as f32 / RATE as f32;
            let env = (t / 0.012).min(1.0) * (-t / 0.22).exp(); // quick, soft attack; gentle fade
            let ph = std::f32::consts::TAU * freq * t;
            *v += (ph.sin() + 0.12 * (2.0 * ph).sin()) * env;
        }
    }
    let peak = s.iter().fold(0f32, |m, v| m.max(v.abs())).max(1e-6);
    let gain = 0.22 * 32767.0 / peak; // quiet: about a fifth of full volume
    let data: Vec<u8> = s.iter().flat_map(|v| ((v * gain) as i16).to_le_bytes()).collect();
    let mut w = Vec::with_capacity(44 + data.len());
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&RATE.to_le_bytes());
    w.extend_from_slice(&(RATE * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data.len() as u32).to_le_bytes());
    w.extend_from_slice(&data);
    w
}

/// Is Windows happy to make noise right now?
pub fn may_sound() -> bool {
    let mut state = 0;
    unsafe { SHQueryUserNotificationState(&mut state) < 0 || state == QUNS_ACCEPTS_NOTIFICATIONS }
}

/// Play the chime (no-op while Windows is in a quiet state). `preview` plays regardless.
pub fn chime(preview: bool) {
    if !preview && !may_sound() {
        return;
    }
    let wav = WAV.get_or_init(chime_wav);
    unsafe { PlaySoundW(wav.as_ptr() as *const u16, std::ptr::null_mut(), SND_MEMORY | SND_ASYNC | SND_NODEFAULT) };
}

/// Windows' own notification sound (for the alert card when "Windows" sound is chosen).
pub fn windows_default() {
    if !may_sound() {
        return;
    }
    let name = crate::win::wide("Notification.Default");
    unsafe { PlaySoundW(name.as_ptr(), std::ptr::null_mut(), SND_ALIAS | SND_ASYNC | SND_NODEFAULT) };
}

#[cfg(test)]
mod tests {
    #[test]
    fn chime_is_a_short_quiet_wav() {
        let w = super::chime_wav();
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(&w[8..12], b"WAVE");
        let samples: Vec<i16> = w[44..].chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        assert!(samples.len() < super::RATE as usize); // under a second
        let peak = samples.iter().map(|v| v.unsigned_abs()).max().unwrap();
        assert!(peak < 8000, "too loud: {peak}");
        assert!(samples.last().unwrap().unsigned_abs() < 200, "should fade out");
    }
}
