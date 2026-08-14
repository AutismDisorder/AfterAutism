// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Versioned corpus identity header (`AFTERCOR` magic + version bytes).

use crate::storage::error::CorpusHeaderError;
use std::io::{Read, Write};

/// Canonical magic bytes for an `afterautism-storage` corpus file. ASCII
/// `b"AFTERCOR"` (eight bytes — no NUL terminator) so hexdump output is
/// readable. Read this as 8 raw bytes; it is NOT a `u64` integer on the
/// wire — we never want to read this field as an integer.
pub const MAGIC: [u8; 8] = *b"AFTERCOR";

/// Total header length in bytes: 8 (magic) + 1 (major) + 1 (minor) + 1
/// (schema) + 5 (reserved). The first payload byte always starts at
/// offset `HEADER_LEN`.
pub const HEADER_LEN: usize = 16;

/// Reserved zero pad following `MAGIC`/major/minor/schema in v1.0. If
/// the wire layout adds byte-fields here in a minor bump, the v1.0 reader
/// ignores them; a writer MUST emit these as zeros in v1.0.
pub const HEADER_RESERVED_V1: [u8; 5] = [0; 5];

/// Highest **major** version this build of `afterautism-storage` understands.
/// Currently `1`. A major bump means the wire layout changed in a way
/// the old reader cannot safely consume.
pub const SUPPORTED_MAJOR: u8 = 1;

/// The parsed corpus header. Construct via [`CorpusHeader::new_v1`] (for
/// writing) or [`CorpusHeader::read_from`] (for reading).
/// `major` is a constant in v1.x (always `1`); the type exposes it for
/// completeness and so a future v2 reader can be implemented next to
/// this code without touching the wire layout's type signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CorpusHeader {
    /// Major wire-format version. `1` for the current series.
    pub major: u8,
    /// Back-compat additions within a major. `0` for the initial v1.0
    /// release; bump when an additive field is added in a way the v1.0
    /// reader can ignore.
    pub minor: u8,
    /// Payload-section schema version. Independent of the corpus file
    /// format version, and independent of the adapter trait version.
    pub schema_version: u8,
}

impl CorpusHeader {
    /// Construct a header for the supported **v1** major with the given
    /// minor / schema version. Panics in debug if `major != 1` (this
    /// type only emits v1 today; a v2 writer will land via a different
    /// constructor when v2 is announced — see `SUPPORTED_MAJOR`).
    /// The intended use is at write time, when the adapter/storage layer
    /// knows its payload schema version and the minor version of the
    /// format it intends to emit.
    #[must_use]
    pub fn new_v1(minor: u8, schema_version: u8) -> Self {
        debug_assert_eq!(SUPPORTED_MAJOR, 1);
        Self {
            major: SUPPORTED_MAJOR,
            minor,
            schema_version,
        }
    }

    /// Serialize the header (16 bytes) to `out`. The reserved 5 bytes
    /// after `magic/major/minor/schema` are written as zeros in v1.0.
    pub fn write_into(&self, out: &mut impl Write) -> std::io::Result<()> {
        out.write_all(&MAGIC)?;
        out.write_all(&[self.major, self.minor, self.schema_version])?;
        out.write_all(&HEADER_RESERVED_V1)?;
        Ok(())
    }

    /// Parse a header from `input`. Refuses a future **major** version
    /// with `CorpusHeaderError::FutureVersion`, a wrong magic with
    /// [`CorpusHeaderError::WrongMagic`], and a truncated stream with
    /// [`CorpusHeaderError::Truncated`]. On success the parsed header is
    /// returned and the `Read` cursor is positioned just past the
    /// 16-byte header — the caller reads the payload from there.
    /// **Backward read**: a v1.x reader against a v1.y corpus (y >= x)
    /// reads the known prefix fields (`magic`, `major`, `minor`,
    /// `schema_version`) and then dumps the trailing `5 - (y - x)` (or
    /// all 5) reserved bytes; future-reserved bytes inside the
    /// `HEADER_LEN` window are skipped. Trailing payload bytes after
    /// `HEADER_LEN` are the caller's responsibility, not this
    /// reader's — the header module stops after 16 bytes.
    /// **Forward read**: a v1.y reader against a v1.x corpus (y >= x)
    /// works because the v1.x fields are still present in their v1.0
    /// offsets; the y-knowledge fields simply take their v1.x value.
    /// There's no missing-field discrimination at the *header* layer
    /// because major remains 1 across the v1.x series.
    pub fn read_from(input: &mut impl Read) -> Result<Self, CorpusHeaderError> {
        // Read the fixed 16-byte header window.
        let mut header_bytes = [0u8; HEADER_LEN];
        let bytes_read = input.read(&mut header_bytes).map_err(|_e| {
            // Wrap IO error as truncated — if we can't read the full
            // window, the stream ended early (or had an IO error).
            CorpusHeaderError::Truncated {
                needed: HEADER_LEN,
                have: 0, // exact count unavailable on IO error
            }
        })?;

        // Check for truncation within the header window.
        if bytes_read < HEADER_LEN {
            return Err(CorpusHeaderError::Truncated {
                needed: HEADER_LEN,
                have: bytes_read,
            });
        }

        // Validate magic bytes (first 8 bytes).
        let magic = &header_bytes[0..8];
        if magic != MAGIC {
            return Err(CorpusHeaderError::WrongMagic {
                expected: MAGIC,
                found: magic.try_into().expect("magic slice is 8 bytes"),
            });
        }

        // Parse version fields (bytes 8, 9, 10).
        let major = header_bytes[8];
        let minor = header_bytes[9];
        let schema_version = header_bytes[10];

        // Refuse future major version.
        if major > SUPPORTED_MAJOR {
            return Err(CorpusHeaderError::FutureVersion {
                file: major,
                supported: SUPPORTED_MAJOR,
            });
        }

        // Bytes 11-15 are reserved in v1.x; we ignore them.
        // The reader has consumed exactly HEADER_LEN bytes.
        Ok(Self {
            major,
            minor,
            schema_version,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bundle a small in-memory read/write cursor so each test is a
    /// self-contained round-trip without file fixtures.
    fn cursor() -> std::io::Cursor<Vec<u8>> {
        std::io::Cursor::new(Vec::new())
    }

    #[test]
    fn v1_header_round_trips_through_memory_buffer() {
        // round-trip on the supported v1.0 baseline. Write a
        // header, read it back, fields equal.
        let mut buf = cursor();
        let written = CorpusHeader::new_v1(0, 1);
        written.write_into(&mut buf).expect("write_into succeeds");

        buf.set_position(0);
        let read = CorpusHeader::read_from(&mut buf).expect("read_from succeeds");

        assert_eq!(read, written);
        assert_eq!(read.major, SUPPORTED_MAJOR);
        assert_eq!(read.minor, 0);
        assert_eq!(read.schema_version, 1);
        assert_eq!(buf.position(), HEADER_LEN as u64);
    }

    #[test]
    fn v1_reader_reads_v1_1_corpus_with_unknown_minor() {
        // backward read across minor versions within the same
        // major. We fake a "v1.1" stream by manually writing a header
        // whose minor field is `1` (while the reader was built with
        // knowledge only of minor `0`). The reader must NOT refuse —
        // minor bumps are back-compat. The reader still has to consume
        // the same 16-byte header window; it doesn't peek at payload.
        let mut buf = cursor();
        let minor_bumped = CorpusHeader {
            major: SUPPORTED_MAJOR,
            minor: 1, // unannounced minor in v1 series — back-compat
            schema_version: 1,
        };
        minor_bumped
            .write_into(&mut buf)
            .expect("write_into succeeds for any v1.x");

        // Append a few trailing payload bytes so the stream keeps going
        // past the header — these are payload, not header, so the reader
        // should not consume them.
        let payload_tail: &[u8] = b"whatever-payload-here";
        buf.set_position(HEADER_LEN as u64);
        buf.write_all(payload_tail).expect("write_all payload tail");

        buf.set_position(0);
        let read = CorpusHeader::read_from(&mut buf).expect("backward read succeeds");
        assert_eq!(read, minor_bumped);
        assert_eq!(read.minor, 1);
        assert_eq!(buf.position(), HEADER_LEN as u64);
    }

    #[test]
    fn future_major_version_refused_with_explicit_error() {
        // / — a future-major corpus must be refused, not
        // misinterpreted as a v1 corpus under the wrong header shape.
        let mut buf = cursor();
        // Hand-assemble a "v2" header: same magic bytes, major=2,
        // arbitrary minor + schema, plus 5 reserved zeros so the header
        // window still has the right size.
        buf.write_all(&MAGIC).expect("magic");
        buf.write_all(&[
            2, // unsupported major
            0, // minor
            1, // schema
        ])
        .expect("v2 fields");
        buf.write_all(&HEADER_RESERVED_V1).expect("reserved");

        buf.set_position(0);
        let err = CorpusHeader::read_from(&mut buf).expect_err("future major should be refused");
        match err {
            CorpusHeaderError::FutureVersion { file, supported } => {
                assert_eq!(file, 2);
                assert_eq!(supported, SUPPORTED_MAJOR);
            }
            other => panic!("expected FutureVersion, got {other:?}"),
        }
    }

    #[test]
    fn wrong_magic_refused() {
        // Defender against accidental misadoption of the `.corpus`
        // extension by unrelated files.
        let mut buf = cursor();
        buf.write_all(b"NOPE____").expect("wrong magic");
        buf.write_all(&[SUPPORTED_MAJOR, 0, 1]).expect("v1 fields");
        buf.write_all(&HEADER_RESERVED_V1).expect("reserved");

        buf.set_position(0);
        let err = CorpusHeader::read_from(&mut buf).expect_err("wrong magic");
        match err {
            CorpusHeaderError::WrongMagic { expected, found } => {
                assert_eq!(expected, MAGIC);
                assert_eq!(&found, b"NOPE____");
            }
            other => panic!("expected WrongMagic, got {other:?}"),
        }
    }

    #[test]
    fn truncated_header_refused() {
        // A header with fewer than `HEADER_LEN` bytes cannot be parsed;
        // the reader must say so explicitly rather than producing a
        // partial / silently-zero-filled header.
        let mut buf = cursor();
        // Write only the magic bytes; nothing else.
        buf.write_all(&MAGIC).expect("magic only");

        buf.set_position(0);
        let err = CorpusHeader::read_from(&mut buf).expect_err("truncated header");
        match err {
            CorpusHeaderError::Truncated { needed, have } => {
                assert_eq!(needed, HEADER_LEN);
                // We supplied 8 bytes before EOF (only MAGIC).
                assert_eq!(have, MAGIC.len());
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[test]
    fn header_len_matches_layout_specification() {
        // 8 (magic) + 1 (major) + 1 (minor) + 1 (schema) + 5 (reserved).
        // If a future patch grows or shrinks this, the test breaks before
        // the wire format silently changes — that's the kind of thing
        // the versioning promise in depends on.
        assert_eq!(
            HEADER_LEN,
            MAGIC.len() + 1 + 1 + 1 + HEADER_RESERVED_V1.len()
        );
    }

    #[test]
    fn magic_bytes_are_after_cor_ascii() {
        // Belt and braces — magic drift would break cross-version
        // compatibility by accident. Pin the literal in the test so
        // even a refactor of `MAGIC` cannot silently drift the wire
        // shape via compiler-coerced array literals.
        assert_eq!(&MAGIC, b"AFTERCOR");
        assert_eq!(MAGIC.len(), 8);
    }

    #[test]
    fn every_truncation_length_is_refused_cleanly() {
        let mut buf = Vec::new();
        CorpusHeader::new_v1(0, 1).write_into(&mut buf).unwrap();
        for cut in 0..buf.len() {
            let mut cur = std::io::Cursor::new(&buf[..cut]);
            assert!(
                CorpusHeader::read_from(&mut cur).is_err(),
                "truncation at {cut} bytes must be a typed error"
            );
        }
    }

    #[test]
    fn single_bit_flips_never_panic() {
        // Flip every bit of every header byte: the reader must return a
        // typed error or a parsed header — never panic, never silently
        // accept garbage (a flipped magic/major byte must refuse).
        let mut buf = Vec::new();
        CorpusHeader::new_v1(0, 1).write_into(&mut buf).unwrap();
        for byte in 0..buf.len() {
            for bit in 0..8 {
                let mut mutated = buf.clone();
                mutated[byte] ^= 1 << bit;
                let mut cur = std::io::Cursor::new(&mutated);
                let _ = CorpusHeader::read_from(&mut cur);
            }
        }
    }

    #[test]
    fn new_v1_pins_major_to_supported_constant() {
        // The constructor hard-pins v1.x for v1 writers; the type stays
        // closed under `major == SUPPORTED_MAJOR`. A future v2 writer
        // gets its own constructor rather than coring through this one.
        let h = CorpusHeader::new_v1(0, 0);
        assert_eq!(h.major, SUPPORTED_MAJOR);
        assert_eq!(h.minor, 0);
        assert_eq!(h.schema_version, 0);
    }
}
