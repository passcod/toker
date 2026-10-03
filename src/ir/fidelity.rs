//! Invariant 5's per-request fidelity monitor.
//!
//! Plan: Invariants 5 — "untransformed requests are passthrough by
//! construction, *verified per request*". The server byte-compares the
//! re-serialised body against the original buffer on every request: bytes
//! match (the normal case), the original buffer is forwarded, identical to
//! what a passthrough proxy would have sent; bytes differ, the original is
//! still forwarded (safe), and a `fidelity-drift` ledger row records route,
//! frontend, and the divergence digest. Drift is a visible, queryable
//! metric, not a hoped-for absence — this module is the metric's source.

use super::short_hash;

/// The outcome of comparing the original request bytes with what the IR
/// re-serialises them to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fidelity {
    /// Byte-identical — the normal case: forward the original buffer.
    Exact,
    /// The bytes differ: still forward the original buffer, and record a
    /// `fidelity-drift` row carrying `offset` and `digest`.
    Drift {
        /// The first differing byte (the divergence point; when one side is
        /// a byte-prefix of the other, the length of the shorter side).
        offset: usize,
        /// sha256/12 over the differing region: the 64 bytes around the
        /// divergence from each side (original window, then serialised
        /// window, clamped at the ends). Equal drifts compare equal;
        /// different drifts almost never collide.
        digest: String,
    },
}

/// Compare the original request bytes with the IR's serialisation. Pure —
/// no state, no clock (invariant 4) — and cheap in the normal case: a
/// length check plus a memcmp short-circuit to [`Fidelity::Exact`] before
/// any window or digest is computed.
pub fn compare(original: &[u8], serialised: &[u8]) -> Fidelity {
    if original == serialised {
        return Fidelity::Exact;
    }
    let offset = original
        .iter()
        .zip(serialised)
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| original.len().min(serialised.len()));
    let mut region = Vec::with_capacity(2 * WINDOW);
    region.extend_from_slice(around(offset, original));
    region.extend_from_slice(around(offset, serialised));
    Fidelity::Drift {
        offset,
        digest: short_hash(&region),
    }
}

/// The `WINDOW` bytes around `offset` in `bytes`: half before, half after,
/// clamped at the ends.
fn around(offset: usize, bytes: &[u8]) -> &[u8] {
    let start = offset.saturating_sub(WINDOW / 2);
    let end = (offset + WINDOW / 2).min(bytes.len());
    &bytes[start..end]
}

const WINDOW: usize = 64;

#[cfg(test)]
mod tests {
    use super::super::short_hash;
    use super::{Fidelity, compare};

    #[test]
    fn identical_bytes_are_exact() {
        let body = b"{\"model\":\"m\",\"messages\":[]}";
        assert_eq!(compare(body, body), Fidelity::Exact);
        assert_eq!(compare(b"", b""), Fidelity::Exact);
    }

    #[test]
    fn drift_reports_the_offset_and_a_digest_of_the_region() {
        let original = b"hello world";
        let serialised = b"hello w0rld";
        // Windows (offset 7, clamped): all of each side.
        let expected = [original.as_slice(), serialised.as_slice()].concat();
        assert_eq!(
            compare(original, serialised),
            Fidelity::Drift {
                offset: 7,
                digest: short_hash(&expected),
            }
        );
    }

    #[test]
    fn a_byte_prefix_drifts_at_the_shorter_length() {
        let original = b"abc";
        let serialised = b"abcd";
        // Windows at offset 3: "abc" from each side, then "d" more from
        // the longer side — the region keeps enough of both to compare.
        let expected = [b"abc".as_slice(), b"abcd".as_slice()].concat();
        assert_eq!(
            compare(original, serialised),
            Fidelity::Drift {
                offset: 3,
                digest: short_hash(&expected),
            }
        );
        match compare(b"", b"x") {
            Fidelity::Drift { offset: 0, .. } => {}
            other => panic!("empty vs non-empty drifts at offset 0, got {other:?}"),
        }
    }

    #[test]
    fn drift_digests_are_deterministic_and_discriminating() {
        let a = compare(b"one", b"two");
        let b = compare(b"one", b"two");
        let c = compare(b"one", b"ten");
        assert_eq!(a, b, "the same drift digests identically");
        assert_ne!(a, c, "different drifts digest differently");
        assert!(matches!(a, Fidelity::Drift { digest, .. } if digest.len() == 12));
    }
}
