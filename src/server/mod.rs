//! Axum listener, routing, and streaming.
//!
//! Plan: "Server core" + "Deployment" — one loopback socket (all frontend
//! protocols plus `/_toker/*`), socket-activated via `listenfd` with a direct
//! bind fallback; buffered request bodies for gating, pass-through response
//! streams with a crash-proof SSE side-parser for usage/model/cost.
