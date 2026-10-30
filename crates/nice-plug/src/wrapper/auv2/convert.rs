// DELTON FORK DELTA: au-1, AUv2 wrapper
//! Four-char codes, the packed component version, and samples-to-seconds conversions. Pure: builds
//! and tests on every OS.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use nice_plug_core::plugin::ProcessStatus;

/// The tail time reported for [`ProcessStatus::KeepAlive`]. AU has no infinite tail, and some hosts
/// mishandle infinity, so this is long enough for every bounce.
pub const KEEP_ALIVE_TAIL_SECONDS: f64 = 3600.0;

/// Big-endian four-char code, as AudioComponentDescription stores it: b"aufx" -> 0x61756678.
pub const fn four_cc(code: [u8; 4]) -> u32 {
    u32::from_be_bytes(code)
}

/// Every byte printable ASCII (0x20..=0x7E).
pub const fn four_cc_valid(code: [u8; 4]) -> bool {
    let mut i = 0;
    while i < 4 {
        if code[i] < 0x20 || code[i] > 0x7E {
            return false;
        }
        i += 1;
    }
    true
}

/// `four_cc_valid` and at least one byte outside b'a'..=b'z' (Apple reserves all-lowercase codes).
pub const fn manufacturer_valid(code: [u8; 4]) -> bool {
    if !four_cc_valid(code) {
        return false;
    }

    let mut i = 0;
    while i < 4 {
        if code[i] < b'a' || code[i] > b'z' {
            return true;
        }
        i += 1;
    }
    false
}

/// samples / sample_rate; 0.0 when sample_rate is not finite or <= 0.
pub fn seconds_from_samples(samples: u32, sample_rate: f64) -> f64 {
    if sample_rate.is_finite() && sample_rate > 0.0 {
        samples as f64 / sample_rate
    } else {
        0.0
    }
}

/// The AU TailTime for the last process status: Tail(n) -> n / sr; KeepAlive ->
/// [`KEEP_ALIVE_TAIL_SECONDS`] regardless of the sample rate; Normal and Error -> 0.0.
pub fn tail_seconds(status: &ProcessStatus, sample_rate: f64) -> f64 {
    match status {
        ProcessStatus::Tail(samples) => seconds_from_samples(*samples, sample_rate),
        ProcessStatus::KeepAlive => KEEP_ALIVE_TAIL_SECONDS,
        ProcessStatus::Normal | ProcessStatus::Error(_) => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_cc_is_big_endian() {
        assert_eq!(four_cc(*b"aufx"), 0x6175_6678);
        assert_eq!(four_cc(*b"Dltn"), 0x446C_746E);
    }

    #[test]
    fn four_cc_validity() {
        assert!(four_cc_valid(*b"DrVb"));
        assert!(!four_cc_valid([b'a', b'b', 0x07, b'c']));
        assert!(!four_cc_valid([b'a', b'b', b'c', 0x7F]));
    }

    #[test]
    fn manufacturer_validity() {
        assert!(manufacturer_valid(*b"Dltn"));
        assert!(!manufacturer_valid(*b"dltn"));
        assert!(manufacturer_valid(*b"dlt1"));
        assert!(!manufacturer_valid([b'D', b'l', 0x07, b'n']));
    }

    #[test]
    fn seconds_from_samples_values() {
        assert!((seconds_from_samples(4410, 44100.0) - 0.1).abs() < 1e-15);
        assert!((seconds_from_samples(64, 96000.0) - 64.0 / 96000.0).abs() < 1e-15);
        assert_eq!(seconds_from_samples(10, 0.0), 0.0);
        assert_eq!(seconds_from_samples(10, f64::NAN), 0.0);
        assert_eq!(seconds_from_samples(10, -44100.0), 0.0);
    }

    #[test]
    fn tail_seconds_values() {
        assert!((tail_seconds(&ProcessStatus::Tail(22050), 44100.0) - 0.5).abs() < 1e-15);
        assert_eq!(tail_seconds(&ProcessStatus::KeepAlive, 44100.0), 3600.0);
        assert_eq!(tail_seconds(&ProcessStatus::KeepAlive, 0.0), 3600.0);
        assert_eq!(tail_seconds(&ProcessStatus::Normal, 44100.0), 0.0);
        assert_eq!(tail_seconds(&ProcessStatus::Error("x"), 44100.0), 0.0);
    }
}
