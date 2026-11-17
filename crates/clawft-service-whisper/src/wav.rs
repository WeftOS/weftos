//! Minimal WAV-wrapping for PCM chunks.
//!
//! The whisper HTTP service (per `whisper-service-api.md` §6 step 2)
//! reads the WAV header to determine sample rate; it rejects raw PCM.
//! We hand-write a 44-byte RIFF header rather than pulling `hound` for
//! what amounts to 12 `u32`s.
//!
//! Format produced is always:
//!
//! - RIFF/WAVE container
//! - `fmt ` chunk of size 16 (PCM, no extension)
//! - AudioFormat = 1 (PCM)
//! - signed 16-bit little-endian samples
//! - mono or stereo per `channels`
//!
//! The server's native rate is 16 kHz mono; we always ship that shape
//! today, but [`try_write_wav`] is parameterised so a future multi-channel
//! or resampled-upstream path doesn't need a rewrite.

/// Bytes per sample for s16le.
const BYTES_PER_SAMPLE: u16 = 2;
/// PCM AudioFormat marker.
const AUDIO_FORMAT_PCM: u16 = 1;
/// Bit depth of each sample.
const BITS_PER_SAMPLE: u16 = 16;

/// Why a PCM payload could not be wrapped in a WAV container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WavError {
    /// The sample rate was zero.
    #[error("wav: sample rate is zero")]
    ZeroSampleRate,
    /// The channel count was zero.
    #[error("wav: channel count is zero")]
    ZeroChannels,
    /// `sample_rate * channels * 2` does not fit the 32-bit header field.
    #[error("wav: byte rate overflows u32 (sample rate {sample_rate} Hz, {channels} channels)")]
    ByteRateOverflow {
        /// Sample rate that was requested.
        sample_rate: u32,
        /// Channel count that was requested.
        channels: u16,
    },
    /// `channels * 2` does not fit the 16-bit block-align field.
    #[error("wav: block align overflows u16 ({channels} channels)")]
    BlockAlignOverflow {
        /// Channel count that was requested.
        channels: u16,
    },
    /// The payload does not fit a RIFF container (4 GiB limit).
    #[error("wav: payload of {len} bytes exceeds the RIFF size limit")]
    PayloadTooLong {
        /// Payload length in bytes.
        len: usize,
    },
}

/// Wrap raw s16le PCM bytes in a WAV container.
///
/// # Arguments
/// * `pcm_s16le`: raw PCM payload; length must be a multiple of
///   `channels * 2` bytes, otherwise the trailing odd byte is dropped
///   from the declared data-chunk size (but kept in the output for
///   whisper to deal with, matching the permissive Python snippet in
///   the service-API doc section 6).
/// * `sample_rate`: Hz (e.g. 16000).
/// * `channels`: channel count (1 for mono, 2 for stereo).
///
/// # Errors
/// Returns [`WavError`] when a header field would overflow or the
/// sample rate or channel count is zero. The inputs come from substrate
/// JSON written by other nodes, so they are not trusted.
pub fn try_write_wav(
    pcm_s16le: &[u8],
    sample_rate: u32,
    channels: u16,
) -> Result<Vec<u8>, WavError> {
    if sample_rate == 0 {
        return Err(WavError::ZeroSampleRate);
    }
    if channels == 0 {
        return Err(WavError::ZeroChannels);
    }
    let data_len = u32::try_from(pcm_s16le.len()).map_err(|_| WavError::PayloadTooLong {
        len: pcm_s16le.len(),
    })?;
    // RIFF size = 4 ("WAVE") + (8 + 16 fmt) + (8 + data_len). Total
    // file size = 8 + RIFF size.
    let riff_size = data_len
        .checked_add(4 + (8 + 16) + 8)
        .ok_or(WavError::PayloadTooLong {
            len: pcm_s16le.len(),
        })?;
    let byte_rate = sample_rate
        .checked_mul(u32::from(channels))
        .and_then(|v| v.checked_mul(u32::from(BYTES_PER_SAMPLE)))
        .ok_or(WavError::ByteRateOverflow {
            sample_rate,
            channels,
        })?;
    let block_align = channels
        .checked_mul(BYTES_PER_SAMPLE)
        .ok_or(WavError::BlockAlignOverflow { channels })?;

    let mut buf = Vec::with_capacity(44usize.saturating_add(pcm_s16le.len()));
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&riff_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");

    // `fmt ` subchunk.
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // subchunk size
    buf.extend_from_slice(&AUDIO_FORMAT_PCM.to_le_bytes());
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&block_align.to_le_bytes());
    buf.extend_from_slice(&BITS_PER_SAMPLE.to_le_bytes());

    // `data` subchunk.
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_len.to_le_bytes());
    buf.extend_from_slice(pcm_s16le);

    Ok(buf)
}

/// Infallible form of [`try_write_wav`].
///
/// # Panics
/// Panics when [`try_write_wav`] would return an error.
#[deprecated(since = "0.8.5", note = "panics on hostile header values; use `try_write_wav`")]
pub fn write_wav(pcm_s16le: &[u8], sample_rate: u32, channels: u16) -> Vec<u8> {
    match try_write_wav(pcm_s16le, sample_rate, channels) {
        Ok(wav) => wav,
        Err(e) => panic!("write_wav: {e}"),
    }
}

/// Parse a WAV file and return its raw PCM body + (sample_rate, channels).
///
/// Forgiving enough to accept the common subset: RIFF/WAVE, PCM
/// (AudioFormat=1), s16le. Rejects anything else with an `Err(reason)`.
///
/// Used only by the [`crate`]'s test harness (`examples/publish_wav.rs`
/// feeds a real WAV file into substrate), not by the runtime pipeline.
pub fn parse_wav(bytes: &[u8]) -> Result<(Vec<u8>, u32, u16), &'static str> {
    if bytes.len() < 44 {
        return Err("wav: too short for header");
    }
    if &bytes[0..4] != b"RIFF" {
        return Err("wav: missing RIFF marker");
    }
    if &bytes[8..12] != b"WAVE" {
        return Err("wav: missing WAVE marker");
    }

    // Walk sub-chunks looking for `fmt ` + `data`. Not every producer
    // puts them back-to-back starting at offset 12 (fmt chunks can have
    // extensions, producers may insert LIST/INFO), so do a proper scan.
    let mut pos = 12usize;
    let mut fmt: Option<(u32, u16, u16)> = None; // (sample_rate, channels, bits)
    let mut audio_format: u16 = 0;
    let mut data: Option<Vec<u8>> = None;

    while pos + 8 <= bytes.len() {
        let tag = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        pos += 8;
        if pos + size > bytes.len() {
            return Err("wav: chunk size exceeds file");
        }
        match tag {
            b"fmt " => {
                if size < 16 {
                    return Err("wav: fmt chunk too small");
                }
                audio_format = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]);
                let channels = u16::from_le_bytes([bytes[pos + 2], bytes[pos + 3]]);
                let sample_rate = u32::from_le_bytes([
                    bytes[pos + 4],
                    bytes[pos + 5],
                    bytes[pos + 6],
                    bytes[pos + 7],
                ]);
                let bits = u16::from_le_bytes([bytes[pos + 14], bytes[pos + 15]]);
                fmt = Some((sample_rate, channels, bits));
            }
            b"data" => {
                data = Some(bytes[pos..pos + size].to_vec());
            }
            _ => {
                // Skip unknown chunks (LIST, INFO, bext, …).
            }
        }
        // Chunks are word-aligned; if size is odd the spec pads to even.
        pos += size + (size & 1);
        if data.is_some() && fmt.is_some() {
            break;
        }
    }

    let (sr, ch, bits) = fmt.ok_or("wav: missing fmt chunk")?;
    let pcm = data.ok_or("wav: missing data chunk")?;
    if audio_format != AUDIO_FORMAT_PCM {
        return Err("wav: only PCM (format=1) supported");
    }
    if bits != BITS_PER_SAMPLE {
        return Err("wav: only 16-bit PCM supported");
    }
    Ok((pcm, sr, ch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_parse_roundtrips() {
        let pcm: Vec<u8> = (0..1000u16).flat_map(|s| s.to_le_bytes()).collect();
        let wav = try_write_wav(&pcm, 16000, 1).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        let (body, sr, ch) = parse_wav(&wav).unwrap();
        assert_eq!(sr, 16000);
        assert_eq!(ch, 1);
        assert_eq!(body, pcm);
    }

    #[test]
    fn header_is_exactly_44_bytes() {
        // For PCM, fmt subchunk size = 16, so the header is 12 (RIFF) +
        // 24 (fmt) + 8 (data header) = 44 bytes before the payload.
        let wav = try_write_wav(&[], 16000, 1).unwrap();
        assert_eq!(wav.len(), 44);
    }

    #[test]
    fn parse_rejects_short_input() {
        assert!(parse_wav(&[0u8; 10]).is_err());
    }

    #[test]
    fn parse_rejects_non_riff() {
        let mut bogus = vec![0u8; 44];
        bogus[0..4].copy_from_slice(b"FUZZ");
        assert!(parse_wav(&bogus).is_err());
    }

    #[test]
    fn stereo_declares_correct_byte_rate() {
        // Stereo @ 16 kHz = 16000 * 2ch * 2B = 64000 B/s. Byte rate is
        // at offset 28..32. Smoke test to catch arithmetic drift.
        let wav = try_write_wav(&[0u8; 4], 16000, 2).unwrap();
        let br = u32::from_le_bytes([wav[28], wav[29], wav[30], wav[31]]);
        assert_eq!(br, 64_000);
    }

    #[test]
    fn hostile_header_values_are_errors_not_panics() {
        assert_eq!(
            try_write_wav(&[], u32::MAX, 2),
            Err(WavError::ByteRateOverflow {
                sample_rate: u32::MAX,
                channels: 2
            })
        );
        assert_eq!(
            try_write_wav(&[], 1, u16::MAX),
            Err(WavError::BlockAlignOverflow { channels: u16::MAX })
        );
        assert_eq!(try_write_wav(&[], 0, 1), Err(WavError::ZeroSampleRate));
        assert_eq!(try_write_wav(&[], 16000, 0), Err(WavError::ZeroChannels));
    }
}
