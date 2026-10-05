//! Intermediate representation: the canonical request model.
//!
//! Plan: "Intermediate representation" — every request is parsed into a
//! canonical internal model so provider-specific extensions survive the
//! trip, middleware transforms the IR, and the backend adapter serialises
//! to the backend's protocol. There is no passthrough code path: passthrough
//! is what the IR produces when nothing transforms it.
//!
//! Design stance, settled for phase 1: the IR *is* a [`serde_json::Value`]
//! (parsed with `preserve_order` + `arbitrary_precision`, both enabled in
//! Cargo.toml), wrapped in [`Request`] with typed protocol-family views
//! layered on top ([`openai_chat`]). The simplification satisfies the same
//! invariants as a hand-modelled canonical type:
//!
//! - **Byte-exactness comes free.** With key order preserved and number
//!   tokens held verbatim (the `arbitrary_precision` backing is the raw
//!   literal), serde_json re-emits a preserved Value without reformatting:
//!   `serialise(parse(body)) == body` for any canonically-encoded body (the
//!   corpus tests prove it per fixture).
//! - **Raw preservation is automatic.** The whole Value is kept, so every
//!   unmodelled field — provider extensions, cache-control markers, unknown
//!   content parts — survives the trip without an explicit "raw" sidecar.
//! - **Mutation goes through typed accessors** ([`Request::openai_chat_mut`]
//!   today), which touch one key position and never remove or reorder fields
//!   they do not understand. Translation adapters (phase 2+) project this
//!   Value into a protocol-typed model when crossing protocols; same-
//!   protocol routes never project at all. The cross-protocol model is
//!   [`canonical`] — the canonical IR frontend adapters parse into and
//!   backend adapters render out of, engaged only on cross-protocol
//!   routes.
//!
//! Invariant 4 (serialisation purity): [`Request::serialise`] is a pure
//! function of the parsed value. No serialisation decision depends on
//! runtime state — meters, gates, clocks — because the type has none to
//! consult: the wrapper holds the Value plus wire provenance only, no
//! cached derived state, and `serialise` reads the Value alone. The only
//! byte changes are deliberate: a middleware transform or a config change,
//! each a user-visible, once-per-change event.
//!
//! Invariant 5 (prefix stability): untransformed requests are passthrough
//! by construction, *verified per request* — the server byte-compares the
//! re-serialised body against the original buffer ([`fidelity::compare`])
//! and records a `fidelity-drift` ledger row when they differ, so drift is
//! a visible, queryable metric, not a hoped-for absence. The property side
//! — appended turns keep the serialised prefix stable — is enforced by the
//! prefix-stability test over deterministic seeded conversations.

pub mod anthropic;
pub mod canonical;
pub mod fidelity;
pub mod openai_chat;
pub mod openai_responses;

pub use anthropic::{
    AnthropicBody, AnthropicBodyMut, AnthropicShape, CompactMarker, PLAN_SENTINEL, Release,
    SENTINEL, System, SystemBlockDigest,
};
pub use canonical::{
    CanonBlock, CanonMessage, CanonRole, CanonTool, CanonToolChoice, CanonicalRequest,
    Capabilities, SamplingSpec, ThinkingSpec, ToolResultContent,
};
pub use fidelity::{Fidelity, compare};
pub use openai_chat::{
    BlockDigest, ChatBody, ChatBodyMut, Content, Message, Messages, Shape, Tool, Tools,
};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// IR errors. Parsing can fail (invalid JSON); serialisation cannot — a
/// parsed Value always serialises.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Result type for IR operations.
pub type Result<T> = std::result::Result<T, Error>;

/// A parsed request: the wire body as a [`serde_json::Value`] plus wire
/// provenance, nothing else.
///
/// `req_bytes` is the length of the original buffer, passed through from
/// [`Request::parse`] for the ledger's shape fields. It is *not* derived
/// state — it is a fact about the wire input — and it is never consulted by
/// [`Request::serialise`], so derived data can never affect serialisation
/// (invariant 4). Protocol adapters validate the shape they need (a
/// chat-completions body is an object with a messages array); `parse` only
/// requires well-formed JSON, so an unparseable body fails in one place.
#[derive(Debug)]
pub struct Request {
    value: Value,
    req_bytes: u64,
}

impl Request {
    /// Parse a request body. Any well-formed JSON is accepted (including
    /// trailing whitespace); protocol adapters layer their own shape checks
    /// on top via the typed views, which read missing keys as absent rather
    /// than erroring.
    pub fn parse(bytes: &[u8]) -> Result<Request> {
        let value = serde_json::from_slice(bytes)?;
        Ok(Request {
            value,
            req_bytes: bytes.len() as u64,
        })
    }

    /// Serialise back to bytes. **Invariant 4**: a pure function of the
    /// parsed Value alone — no clock, no meters, no state, nothing cached —
    /// so the same parsed request always produces the same bytes.
    pub fn serialise(&self) -> Vec<u8> {
        // infallible: a Value that came from `parse` always serialises.
        serde_json::to_vec(&self.value).expect("a parsed Value always serialises")
    }

    /// The parsed body, for reads the typed views do not model yet.
    pub fn value(&self) -> &Value {
        &self.value
    }

    /// The original wire buffer's length, recorded at parse for the ledger.
    pub fn req_bytes(&self) -> u64 {
        self.req_bytes
    }

    /// Replace the parsed body wholesale: the commit point of an
    /// all-or-nothing middleware transform (the compaction retarget is the
    /// first user). The transform is built over a copy and committed only
    /// on success, so a declined transform never touches the request.
    ///
    /// **Invariant 4** is unaffected by construction: `serialise` remains
    /// a pure function of the value it holds — this only changes which
    /// value that is, as a deliberate, user-visible, once-per-change event
    /// (a middleware transform). `req_bytes` keeps the ORIGINAL wire
    /// length: it is a fact about the wire input, and the ledger's shape
    /// fields for a transformed request are the pre-transform shape's.
    pub fn replace_value(&mut self, value: Value) {
        self.value = value;
    }
}

/// `sha256(data)`, truncated to its first 12 hex chars — the row digest
/// shape the ledger has always used.
/// Digests and lengths only, never content (invariant 1).
pub(crate) fn short_hash(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut hex = String::with_capacity(12);
    for byte in digest.iter().take(6) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::{Error, Request};

    #[test]
    fn parse_rejects_invalid_json() {
        for bad in [
            &b""[..],
            &b"{"[..],
            &b"nul"[..],
            &b"{\"a\":1,}"[..],
            &b"[1,]"[..],
        ] {
            assert!(
                matches!(Request::parse(bad), Err(Error::Json(_))),
                "{bad:?} must not parse"
            );
        }
    }

    #[test]
    fn parse_records_the_wire_length_and_round_trips() {
        let body = br#"{"model":"m","messages":[]}"#;
        let request = Request::parse(body).expect("parse");
        assert_eq!(request.req_bytes(), body.len() as u64);
        assert_eq!(request.serialise(), body);
        assert_eq!(
            request.value().get("model").and_then(|v| v.as_str()),
            Some("m")
        );
    }

    #[test]
    fn serialise_has_no_state_to_consult() {
        // The wrapper holds the Value plus wire provenance only, and
        // `serialise` reads the Value alone — purity is structural, not
        // behavioural: there is no field a meter or clock could hide in.
        let body = br#"{"a":1,"b":[true,null,"x"]}"#;
        let first = Request::parse(body).expect("parse");
        let second = Request::parse(body).expect("parse");
        assert_eq!(first.serialise(), second.serialise());
        assert_eq!(first.serialise(), body);
    }

    #[test]
    fn trailing_whitespace_parses_but_is_not_reproduced() {
        // Legal JSON, non-canonical bytes: the fidelity monitor's domain
        // (the drift tests in tests/ cover the reporting).
        let padded = b"{\"a\":1} \n";
        let request = Request::parse(padded).expect("trailing whitespace is legal JSON");
        assert_eq!(request.serialise(), b"{\"a\":1}");
        assert_eq!(request.req_bytes(), padded.len() as u64);
    }
}
