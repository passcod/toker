//! Frontend protocol adapters.
//!
//! Plan: "Architecture — frontend protocol adapters" — all exposed on one
//! loopback port (paths don't collide): Anthropic Messages, OpenAI Chat, and
//! OpenAI Responses, plus the `/_toker/*` control endpoint. Each adapter parses
//! its wire format into the IR and serialises the IR back out.

pub mod anthropic;
pub mod openai_chat;
pub mod openai_responses;
