//! Intermediate representation: the canonical request model.
//!
//! Plan: "Intermediate representation" — every request is parsed into a
//! canonical internal model (messages/turns, tools, system prompt, sampling
//! params, cache directives) with raw preservation of all unmodelled fields so
//! provider-specific extensions survive the trip. There is no passthrough code
//! path: passthrough is what the IR produces when nothing transforms it.

/// A parsed request in toker's canonical model.
#[allow(dead_code)]
#[derive(Debug)]
pub struct Request {
    /// Placeholder; grows into the canonical message/tool/sampling model plus
    /// raw unmodelled fields.
    pub placeholder: (),
}

#[cfg(test)]
mod tests {
    #[test]
    fn ir_skeleton() {
        // Round-trip byte-equality corpus tests land here (invariant 5).
        assert!(true);
    }
}
