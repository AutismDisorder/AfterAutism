// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Payload compression for corpus storage.
//!
//! Payloads are zstd-compressed on write and decompressed on read;
//! short payloads take a raw fast path. Wire format:
//! - `0x00` prefix + raw bytes — short payload (fast path)
//! - `0x01` prefix + zstd frame — compressed payload

use thiserror::Error;

/// Errors from the compression layer.
#[derive(Debug, Error)]
pub enum CompressionError {
    #[error("zstd compress failed: {0}")]
    Compress(String),
    #[error("zstd decompress failed: {0}")]
    Decompress(String),
    #[error("unknown payload marker byte: {0:#04x}")]
    UnknownMarker(u8),
    /// Decompressed payload exceeds the safety ceiling (bomb guard).
    #[error("decompressed payload too large: {0} bytes")]
    Oversized(usize),
}

/// Compress a payload. Short inputs are stored raw.
pub fn compress(text: &[u8], fast_path_max: usize) -> Result<Vec<u8>, CompressionError> {
    if text.len() <= fast_path_max {
        let mut out = Vec::with_capacity(text.len() + 1);
        out.push(0x00);
        out.extend_from_slice(text);
        return Ok(out);
    }
    // Single-shot bulk compression: the payload is already fully in
    // memory, so the streaming encoder's read/write machinery would be
    // pure overhead. Produces a standard zstd frame (same wire format as
    // the previous `encode_all` path), so stored payloads stay readable.
    let frame =
        zstd::bulk::compress(text, 3).map_err(|e| CompressionError::Compress(e.to_string()))?;
    let mut out = Vec::with_capacity(frame.len() + 1);
    out.push(0x01);
    out.extend_from_slice(&frame);
    Ok(out)
}

/// Decompress a payload produced by [`compress`].
/// `max_out` bounds the decompressed size (decompression-bomb guard).
pub fn decompress(data: &[u8], max_out: usize) -> Result<Vec<u8>, CompressionError> {
    let Some((&marker, rest)) = data.split_first() else {
        return Ok(Vec::new());
    };
    match marker {
        0x00 => Ok(rest.to_vec()),
        0x01 => {
            let out =
                zstd::decode_all(rest).map_err(|e| CompressionError::Decompress(e.to_string()))?;
            if out.len() > max_out {
                return Err(CompressionError::Oversized(out.len()));
            }
            Ok(out)
        }
        other => Err(CompressionError::UnknownMarker(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_payload_uses_fast_path() {
        let raw = b"tiny";
        let enc = compress(raw, 64).expect("compress");
        assert_eq!(enc[0], 0x00, "marker says raw");
        assert_eq!(&enc[1..], raw);
        assert_eq!(decompress(&enc, 1024).expect("decompress"), raw);
    }

    #[test]
    fn long_payload_compresses_and_roundtrips() {
        // Repetitive text compresses well; verify the marker is 0x01 and
        // the round-trip is lossless.
        let raw = vec![b'a'; 10_000];
        let enc = compress(&raw, 64).expect("compress");
        assert_eq!(enc[0], 0x01, "marker says zstd");
        assert!(
            enc.len() < raw.len(),
            "compressed smaller: {} < {}",
            enc.len(),
            raw.len()
        );
        assert_eq!(decompress(&enc, 1_000_000).expect("decompress"), raw);
    }

    #[test]
    fn empty_payload_roundtrips() {
        let enc = compress(b"", 64).expect("compress");
        assert!(decompress(&enc, 1024).expect("decompress").is_empty());
    }

    #[test]
    fn oversized_decompression_is_refused() {
        let raw = vec![b'x'; 5_000];
        let enc = compress(&raw, 16).expect("compress");
        let err = decompress(&enc, 1_000).expect_err("exceeds bound");
        assert!(matches!(err, CompressionError::Oversized(_)));
    }

    #[test]
    fn unknown_marker_errors() {
        let err = decompress(&[0x7f, 1, 2, 3], 1024).expect_err("bad marker");
        assert!(matches!(err, CompressionError::UnknownMarker(0x7f)));
    }
}
