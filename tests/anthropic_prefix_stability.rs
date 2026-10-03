//! Invariant 5's prefix-stability property for the Anthropic Messages
//! frontend, in the same seeded style as the openai_chat one
//! (tests/ir_prefix_stability.rs): 20 deterministic seeded cases, each a
//! canonical conversation body with `messages` as the last key, appended
//! through the IR's typed mutator (a tail-only mutation), asserting the
//! serialised bytes are identical up to the point where the appended
//! message begins. Anthropic's top-level `system`/`tools` sit before
//! `messages`, so they exercise the "prefix before the tail never moves"
//! claim against fields the upstream caches first (tools → system →
//! messages).

mod common;

use common::{Rng, WORDS, anthropic_conversation_body};
use toker::ir::Request;

/// A word-list phrase for an appended message, from the generator's pool.
fn phrase(rng: &mut Rng) -> String {
    (0..1 + rng.below(4) as usize)
        .map(|_| rng.pick(WORDS))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The seeded coin's pick of role for an appended message.
fn role(rng: &mut Rng) -> &'static str {
    if rng.below(2) == 0 {
        "user"
    } else {
        "assistant"
    }
}

#[test]
fn appending_a_message_leaves_the_serialised_prefix_stable() {
    for case in 0..20u64 {
        let mut rng = Rng::seeded(0x5EED_A000 + case);
        let base_count = 2 + rng.below(6) as usize;
        let base = anthropic_conversation_body(&mut rng, base_count);

        let mut request = Request::parse(&base).unwrap_or_else(|e| panic!("case {case}: {e}"));
        let short = request.serialise();
        assert_eq!(
            short, base,
            "case {case}: canonical input round-trips (precondition)"
        );

        let appended = 1 + rng.below(3) as usize;
        for _ in 0..appended {
            request
                .anthropic_mut()
                .push_message(role(&mut rng), &phrase(&mut rng));
        }
        let long = request.serialise();

        assert!(
            short.ends_with(b"]}"),
            "case {case}: generated bodies keep `messages` as the last key"
        );
        let cut = short.len() - 2;
        assert_eq!(
            &short[..cut],
            &long[..cut],
            "case {case}: identical up to where the appended message begins"
        );
        assert_eq!(
            long[cut], b',',
            "case {case}: the change starts exactly at the insertion point"
        );
        assert!(long.ends_with(b"]}"), "case {case}: only the tail grew");
        assert_eq!(
            request.anthropic().messages().len(),
            base_count + appended,
            "case {case}: exactly the appended messages were added"
        );
    }
}
