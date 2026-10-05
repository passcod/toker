//! OpenAI Responses protocol marker.
//!
//! The native frontend is served by [`crate::server::codex::responses`]; its
//! typed, content-free request view lives in [`crate::ir::openai_responses`],
//! and the shared SSE observer lives in [`crate::providers::codex`]. Same-wire
//! traffic never enters the canonical translation layer.
