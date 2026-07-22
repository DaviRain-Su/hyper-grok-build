//! PCM16 peak-level helpers for silence detection.
//!
//! Denied mic permission (macOS feeds unauthorized apps zeros), muted input, or
//! a dead device yields ~zero peak. A working mic still has a noise floor, so
//! peak separates "mic misconfigured" from "user didn't speak".

/// Peaks at or below this many PCM16 counts count as digital silence.
/// Small allowance for dither on an otherwise dead input; far below a real
/// mic's noise floor.
pub const SILENCE_PEAK_MAX: u16 = 3;

/// Peak absolute sample of little-endian mono PCM16. Empty → 0; trailing odd
/// byte ignored. `u16` because `i16::MIN.unsigned_abs()` is 32768.
pub fn peak_abs_i16_le(pcm_le: &[u8]) -> u16 {
    pcm_le
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]).unsigned_abs())
        .max()
        .unwrap_or(0)
}

/// [`peak_abs_i16_le`] for samples not yet encoded as bytes.
pub fn peak_abs_i16(samples: &[i16]) -> u16 {
    samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0)
}

/// Whether a peak is digital silence (see [`SILENCE_PEAK_MAX`]).
pub fn is_silence(peak: u16) -> bool {
    peak <= SILENCE_PEAK_MAX
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn peak_abs_i16_le_edges() {
        assert_eq!(peak_abs_i16_le(&[]), 0);
        assert_eq!(peak_abs_i16_le(&pcm(&[10, -500, 300])), 500);
        assert_eq!(peak_abs_i16_le(&pcm(&[i16::MIN])), 32768);
        let mut bytes = pcm(&[7]);
        bytes.push(0xFF);
        assert_eq!(peak_abs_i16_le(&bytes), 7);
    }

    #[test]
    fn peak_abs_i16_edges() {
        assert_eq!(peak_abs_i16(&[]), 0);
        assert_eq!(peak_abs_i16(&[10, -500, 300]), 500);
        assert_eq!(peak_abs_i16(&[i16::MIN]), 32768);
    }

    #[test]
    fn silence_threshold_boundary() {
        assert!(is_silence(0));
        assert!(is_silence(SILENCE_PEAK_MAX));
        assert!(!is_silence(SILENCE_PEAK_MAX + 1));
    }
}
