//! The context-window catalogue — port of the predecessor proxy's
//! model-capability table (capabilities, id aliases, window resolution).
//!
//! Verified 2026-09-27 against Anthropic's context-windows and
//! release-notes pages and OpenAI Codex's models.json (the source list;
//! see [`VERIFIED_ON`]).
//!
//! Keep identities exact. A future model that happens to share a family
//! name is not evidence that it inherited an earlier version's context
//! ceiling: unknown stays `Unknown` rather than confidently wrong
//! (invariant 3). `phases` describe time-bounded request metadata that
//! selected a different limit for historical rows; they are source
//! capability, not persisted observation data.

use serde_json::Value;

/// The date this catalogue was last verified against the providers' pages.
pub const VERIFIED_ON: &str = "2026-09-27";

/// The retired request beta that selected a 1M window on the models that
/// once had one.
pub const CONTEXT_1M_BETA: &str = "context-1m-2025-08-07";

/// A declared context window: `{default, max}` in tokens
/// (the accepted declaration shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRange {
    /// The window a request gets without selecting anything.
    pub default_tokens: u64,
    /// The largest window the provider declares for the model.
    pub max_tokens: u64,
}

/// One beta-selected window variant —
/// always a single beta per capability in the hand-verified catalogue..
#[derive(Debug, Clone, Copy)]
pub struct Variant {
    /// The `anthropic-beta` value that selects this window.
    pub beta: &'static str,
    /// The window the beta selects.
    pub tokens: u64,
}

/// One capability entry's window facts (the capability object minus
/// `phases`).
#[derive(Debug, Clone, Copy)]
pub struct Capability {
    /// The window without (and, for a beta variant, at most) any selection.
    pub context_window: WindowRange,
    /// The beta-selected window variant; `None` when the window is not
    /// selectable.
    pub variant: Option<Variant>,
}

/// One dated phase of a capability: `from`
/// inclusive, `until` exclusive, ISO dates.
#[derive(Debug, Clone, Copy)]
pub struct Phase {
    pub from: Option<&'static str>,
    pub until: Option<&'static str>,
    pub capability: Capability,
}

/// One limit, default and maximum.
const fn fixed(tokens: u64) -> Capability {
    Capability {
        context_window: WindowRange {
            default_tokens: tokens,
            max_tokens: tokens,
        },
        variant: None,
    }
}

/// A default window the listed beta
/// raises to `tokens`.
const fn beta_window(default: u64, beta: &'static str, tokens: u64) -> Capability {
    Capability {
        context_window: WindowRange {
            default_tokens: default,
            max_tokens: tokens,
        },
        variant: Some(Variant { beta, tokens }),
    }
}

/// A provider declaration whose default is
/// lower than its maximum (only the maximum is a ceiling).
const fn declared(default: u64, max: u64) -> Capability {
    Capability {
        context_window: WindowRange {
            default_tokens: default,
            max_tokens: max,
        },
        variant: None,
    }
}

/// One dated phase: `during(from, until, capability)`.
const fn during(
    from: Option<&'static str>,
    until: Option<&'static str>,
    capability: Capability,
) -> Phase {
    Phase {
        from,
        until,
        capability,
    }
}

// Historical phases retain what captured rows prove without pretending the
// retired beta still changes current requests. Opus 4.6 and Sonnet 4.6
// moved from beta selection to native 1M on 2026-03-13.
static SONNET_4_PHASES: &[Phase] = &[
    during(None, Some("2025-08-12"), fixed(200_000)),
    during(
        Some("2025-08-12"),
        Some("2026-04-30"),
        beta_window(200_000, CONTEXT_1M_BETA, 1_000_000),
    ),
    during(Some("2026-04-30"), None, fixed(200_000)),
];
static SONNET_4_5_PHASES: &[Phase] = &[
    during(
        Some("2025-09-29"),
        Some("2026-04-30"),
        beta_window(200_000, CONTEXT_1M_BETA, 1_000_000),
    ),
    during(Some("2026-04-30"), None, fixed(200_000)),
];
static OPUS_4_6_PHASES: &[Phase] = &[
    during(
        Some("2026-02-05"),
        Some("2026-03-13"),
        beta_window(200_000, CONTEXT_1M_BETA, 1_000_000),
    ),
    during(Some("2026-03-13"), None, fixed(1_000_000)),
];
static SONNET_4_6_PHASES: &[Phase] = &[
    during(
        Some("2026-02-17"),
        Some("2026-03-13"),
        beta_window(200_000, CONTEXT_1M_BETA, 1_000_000),
    ),
    during(Some("2026-03-13"), None, fixed(1_000_000)),
];

// One catalogue entry: either a single current capability, or a phased one
// whose current capability is the last phase.
#[derive(Debug, Clone, Copy)]
enum Entry {
    Current(Capability),
    Phased {
        current: Capability,
        phases: &'static [Phase],
    },
}

/// The hand-verified catalogue, keyed by exact normalised identity.
static CATALOG: &[(&str, Entry)] = &[
    // Anthropic's current native 1M models. The beta header does not
    // select their window; 1M is both the default and maximum.
    ("claude-fable-5-1", Entry::Current(fixed(1_000_000))),
    ("claude-mythos-5-1", Entry::Current(fixed(1_000_000))),
    ("claude-fable-5", Entry::Current(fixed(1_000_000))),
    ("claude-mythos-5", Entry::Current(fixed(1_000_000))),
    ("claude-opus-5-5", Entry::Current(fixed(1_000_000))),
    ("claude-opus-5", Entry::Current(fixed(1_000_000))),
    ("claude-opus-4-8", Entry::Current(fixed(1_000_000))),
    ("claude-opus-4-7", Entry::Current(fixed(1_000_000))),
    ("claude-sonnet-5", Entry::Current(fixed(1_000_000))),
    ("claude-mythos-preview", Entry::Current(fixed(1_000_000))),
    // Phased: see the phase tables above.
    (
        "claude-sonnet-4-0",
        Entry::Phased {
            current: fixed(200_000),
            phases: SONNET_4_PHASES,
        },
    ),
    (
        "claude-sonnet-4-5",
        Entry::Phased {
            current: fixed(200_000),
            phases: SONNET_4_5_PHASES,
        },
    ),
    (
        "claude-opus-4-6",
        Entry::Phased {
            current: fixed(1_000_000),
            phases: OPUS_4_6_PHASES,
        },
    ),
    (
        "claude-sonnet-4-6",
        Entry::Phased {
            current: fixed(1_000_000),
            phases: SONNET_4_6_PHASES,
        },
    ),
    // Other catalogued Claude identities have one fixed 200k limit.
    ("claude-opus-4-5", Entry::Current(fixed(200_000))),
    ("claude-opus-4-1", Entry::Current(fixed(200_000))),
    ("claude-opus-4-0", Entry::Current(fixed(200_000))),
    ("claude-haiku-4-5", Entry::Current(fixed(200_000))),
    ("claude-haiku-3-5", Entry::Current(fixed(200_000))),
    ("claude-3-7-sonnet", Entry::Current(fixed(200_000))),
    ("claude-3-5-sonnet", Entry::Current(fixed(200_000))),
    ("claude-3-5-haiku", Entry::Current(fixed(200_000))),
    ("claude-3-opus", Entry::Current(fixed(200_000))),
    ("claude-3-sonnet", Entry::Current(fixed(200_000))),
    ("claude-3-haiku", Entry::Current(fixed(200_000))),
    // OpenAI Codex's catalogue distinguishes its default from its maximum.
    // Only the maximum is displayed, as a declared provider ceiling.
    ("gpt-5.6-sol", Entry::Current(declared(272_000, 872_000))),
    ("gpt-5.6-luna", Entry::Current(declared(272_000, 872_000))),
];

// Pre-4.6 Claude API snapshots used dated IDs. Match only published IDs
// here: stripping an arbitrary future-looking date would turn an unknown
// identity into a known dateless 4.6+ model.
static ALIASES: &[(&str, &str)] = &[
    ("claude-opus-4-5-20251101", "claude-opus-4-5"),
    ("claude-opus-4-1-20250805", "claude-opus-4-1"),
    ("claude-opus-4-20250514", "claude-opus-4-0"),
    ("claude-opus-4", "claude-opus-4-0"),
    ("claude-sonnet-4-5-20250929", "claude-sonnet-4-5"),
    ("claude-sonnet-4-20250514", "claude-sonnet-4-0"),
    ("claude-sonnet-4", "claude-sonnet-4-0"),
    ("claude-haiku-4-5-20251001", "claude-haiku-4-5"),
    ("claude-3-7-sonnet-20250219", "claude-3-7-sonnet"),
    ("claude-3-5-sonnet-20240620", "claude-3-5-sonnet"),
    ("claude-3-5-sonnet-20241022", "claude-3-5-sonnet"),
    ("claude-3-5-haiku-20241022", "claude-3-5-haiku"),
    ("claude-3-opus-20240229", "claude-3-opus"),
    ("claude-3-sonnet-20240229", "claude-3-sonnet"),
    ("claude-3-haiku-20240307", "claude-3-haiku"),
];

/// A provider's context-window declaration as stored alongside a learned
/// model (`{default, max}` JSON; validated
/// before it counts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclaredWindow {
    pub default_tokens: u64,
    pub max_tokens: u64,
}

impl DeclaredWindow {
    /// Read a stored declaration, rejecting every shape that is not a valid
    /// range: non-objects, missing or fractional or non-positive bounds,
    /// and a default above the maximum all read as absent (a dropped
    /// declaration costs a ceiling; a fabricated one would be a lie).
    pub fn from_json(value: &Value) -> Option<DeclaredWindow> {
        let object = value.as_object()?;
        let default_tokens = integer_at(object, "default")?;
        let max_tokens = integer_at(object, "max")?;
        if default_tokens == 0 || max_tokens == 0 || default_tokens > max_tokens {
            return None;
        }
        Some(DeclaredWindow {
            default_tokens,
            max_tokens,
        })
    }
}

/// The resolved context evidence for one served response
/// (`{kind, tokens}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextWindow {
    /// Source-owned fact: the model's ceiling, no request metadata needed.
    Exact { tokens: u64 },
    /// Provider declaration: a ceiling the source states but does not
    /// treat as a fixed capability.
    Declared { tokens: u64 },
    /// No context evidence; the caller reports `?`, never a guess.
    Unknown,
}

impl ContextWindow {
    /// The `kind` string: `exact` | `declared` | `unknown`.
    pub fn kind(&self) -> &'static str {
        match self {
            ContextWindow::Exact { .. } => "exact",
            ContextWindow::Declared { .. } => "declared",
            ContextWindow::Unknown => "unknown",
        }
    }

    /// The ceiling in tokens; `None` for `Unknown`.
    pub fn tokens(&self) -> Option<u64> {
        match self {
            ContextWindow::Exact { tokens } | ContextWindow::Declared { tokens } => Some(*tokens),
            ContextWindow::Unknown => None,
        }
    }
}

/// The exact catalogue identity of a model:
/// lower-cased, client-only bracket variants stripped,
/// published snapshot aliases folded. Unlike pricing's
/// [`normalise_model_id`](super::pricing::normalise_model_id), an
/// *unpublished* trailing date is not stripped — a future identity must not
/// become a known model.
pub fn model_identity(model: &str) -> Option<String> {
    let lowered = strip_brackets(&model.to_ascii_lowercase());
    let id = lowered.trim();
    if id.is_empty() {
        return None;
    }
    Some(
        ALIASES
            .iter()
            .find(|(from, _)| *from == id)
            .map(|(_, to)| (*to).to_owned())
            .unwrap_or_else(|| id.to_owned()),
    )
}

/// Resolve the context evidence for one served response.
///
/// - `model`: the served model id (response identity is authoritative).
/// - `betas`: the request's captured `anthropic-beta` values, `None` when
///   the request carried none or the row did not capture them. A
///   beta-selectable phase without captured betas stays `Unknown` — an
///   uncaptured selection is not evidence.
/// - `fetched`: this model's context window from the row's provider's
///   **fetched catalogue** ([`crate::catalog::fetched`]) — the models
///   API listing, when it named one. Consulted SECOND: after the
///   hand-verified catalogue, before a stored declaration — a fresh
///   provider listing outranks a stale learned one, and a model the
///   hand-verified catalogue KNOWS keeps its verdict even when that
///   verdict is `Unknown` (an uncaptured beta phase is a decision, not
///   a gap a listing may fill). A fetched window resolves as
///   [`ContextWindow::Declared`] — a provider listing is a declaration,
///   not hand-verified source capability.
/// - `declared`: the provider's stored `{default, max}` declaration for a
///   model outside this catalogue (learned-store JSON; validated here).
///   A catalogued model ignores it — the hand-verified table wins over a
///   stale declaration.
/// - `at`: when the row was served, ISO date or timestamp. `None` resolves
///   the model's *current* capability; a date selects the historical phase
///   that applied. A date that parses but selects no phase — an invalid
///   date, or one outside every phase's bounds — is `Unknown`, the same
///   verdict as no evidence.
pub fn resolve_context_window(
    model: &str,
    betas: Option<&[&str]>,
    fetched: Option<u64>,
    declared: Option<&Value>,
    at: Option<&str>,
) -> ContextWindow {
    let Some(id) = model_identity(model) else {
        return ContextWindow::Unknown;
    };
    let Some(entry) = catalog_entry(&id) else {
        // Outside the catalogue: the fetched listing first (fresher
        // than a stored declaration), then a validated provider
        // declaration — both ceilings; a name prefix alone never
        // supplies one.
        if let Some(tokens) = fetched {
            return ContextWindow::Declared { tokens };
        }
        return match declared.and_then(DeclaredWindow::from_json) {
            Some(declared) => ContextWindow::Declared {
                tokens: declared.max_tokens,
            },
            None => ContextWindow::Unknown,
        };
    };

    // Catalogued: the hand-verified verdict is the whole answer — every
    // Unknown it can return (a date no phase covers, an uncaptured
    // beta selection) is a decision about what a listing cannot know,
    // so `fetched` is deliberately NOT consulted past this point.
    let Some(capability) = capability_at(entry, at) else {
        return ContextWindow::Unknown;
    };
    let window = capability.context_window;
    if let Some(variant) = capability.variant {
        // A beta-selectable phase: only a captured beta array can say which
        // window applied; the default is what a captured-empty selection
        // fell back to.
        let Some(betas) = betas else {
            return ContextWindow::Unknown;
        };
        let tokens = if betas.contains(&variant.beta) {
            variant.tokens
        } else {
            window.default_tokens
        };
        return ContextWindow::Exact { tokens };
    }
    if window.default_tokens == window.max_tokens {
        ContextWindow::Exact {
            tokens: window.max_tokens,
        }
    } else {
        ContextWindow::Declared {
            tokens: window.max_tokens,
        }
    }
}

fn catalog_entry(id: &str) -> Option<Entry> {
    CATALOG
        .iter()
        .find(|(key, _)| *key == id)
        .map(|(_, entry)| *entry)
}

/// The phase that applied at `at`, the current
/// capability when no date was given, and `None` for a date that selects no
/// phase (or does not parse).
fn capability_at(entry: Entry, at: Option<&str>) -> Option<Capability> {
    match entry {
        Entry::Current(capability) => Some(capability),
        Entry::Phased { current, phases } => {
            let Some(at) = at else {
                return Some(current);
            };
            let day = iso_day(at)?;
            phases
                .iter()
                .find(|phase| {
                    (phase.from.is_none_or(|from| day >= from))
                        && (phase.until.is_none_or(|until| day < until))
                })
                .map(|phase| phase.capability)
        }
    }
}

/// The date part of a served-at value, validated as `YYYY-MM-DD`
/// (slice-then-shape); anything else selects no phase.
fn iso_day(at: &str) -> Option<&str> {
    let day = at.get(..10)?;
    let bytes = day.as_bytes();
    let digits = |slice: &[u8]| slice.iter().all(u8::is_ascii_digit);
    (bytes.len() == 10
        && digits(&bytes[..4])
        && bytes[4] == b'-'
        && digits(&bytes[5..7])
        && bytes[7] == b'-'
        && digits(&bytes[8..]))
    .then_some(day)
}

/// A strictly-integer positive field, JS `Number.isInteger` semantics:
/// fractional, negative, and string numbers
/// are all invalid.
fn integer_at(object: &serde_json::Map<String, Value>, key: &str) -> Option<u64> {
    object.get(key).and_then(Value::as_u64).filter(|&v| v > 0)
}

/// The bracket strip, shared with pricing: remove
/// complete `[...]` groups, keep an unterminated `[` as data.
fn strip_brackets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        match after.find(']') {
            Some(close) => {
                out.push_str(&rest[..open]);
                rest = &after[close + 1..];
            }
            None => {
                out.push_str(&rest[..open + 1]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::{
        ContextWindow, DeclaredWindow, VERIFIED_ON, model_identity, resolve_context_window,
    };
    use serde_json::json;

    const BETA: &[&str] = &["context-1m-2025-08-07"];
    const NO_BETAS: &[&str] = &[];

    fn exact(tokens: u64) -> ContextWindow {
        ContextWindow::Exact { tokens }
    }

    fn declared(tokens: u64) -> ContextWindow {
        ContextWindow::Declared { tokens }
    }

    #[test]
    fn verified_on_is_pinned_to_the_verified_date() {
        assert_eq!(VERIFIED_ON, "2026-09-27");
        assert!(
            VERIFIED_ON.len() == 10 && VERIFIED_ON.as_bytes()[4] == b'-',
            "YYYY-MM-DD shaped"
        );
    }

    #[test]
    fn current_claude_models_resolve_their_native_1m_limit_without_a_beta() {
        let native = [
            "claude-fable-5-1",
            "claude-mythos-5-1",
            "claude-fable-5",
            "claude-mythos-5",
            "claude-opus-5-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-sonnet-5",
            "claude-mythos-preview",
        ];
        for model in native {
            assert_eq!(
                resolve_context_window(model, None, None, None, None),
                exact(1_000_000),
                "{model}"
            );
            // Captured empty betas change nothing: the beta header does not
            // select these windows.
            assert_eq!(
                resolve_context_window(model, Some(NO_BETAS), None, None, None),
                exact(1_000_000),
                "{model} with captured empty betas"
            );
            // An Anthropic-shaped beta never narrows or widens a native 1M
            // model either.
            assert_eq!(
                resolve_context_window(model, Some(BETA), None, None, None),
                exact(1_000_000),
                "{model} with the 1M beta present"
            );
        }
    }

    #[test]
    fn only_applicable_legacy_phases_use_captured_beta_selection() {
        let historical = "2026-03-01T12:00:00Z";
        for model in ["claude-sonnet-4-0", "claude-sonnet-4-5"] {
            assert_eq!(
                resolve_context_window(model, Some(NO_BETAS), None, None, Some(historical)),
                exact(200_000),
                "{model} without beta"
            );
            assert_eq!(
                resolve_context_window(model, Some(BETA), None, None, Some(historical)),
                exact(1_000_000),
                "{model} with beta"
            );
            assert_eq!(
                resolve_context_window(model, None, None, None, Some(historical)),
                ContextWindow::Unknown,
                "{model} without beta capture"
            );
            assert_eq!(
                resolve_context_window(model, Some(BETA), None, None, Some("2026-05-01T00:00:00Z")),
                exact(200_000),
                "{model} after beta retirement"
            );
            assert_eq!(
                resolve_context_window(model, Some(BETA), None, None, None),
                exact(200_000),
                "{model} current capability"
            );
        }

        // Opus 4.6: beta selection, then native 1M from 2026-03-13.
        assert_eq!(
            resolve_context_window(
                "claude-opus-4-6",
                Some(NO_BETAS),
                None,
                None,
                Some("2026-02-20T00:00:00Z")
            ),
            exact(200_000),
            "beta phase default"
        );
        assert_eq!(
            resolve_context_window(
                "claude-opus-4-6",
                Some(BETA),
                None,
                None,
                Some("2026-02-20T00:00:00Z")
            ),
            exact(1_000_000),
            "beta phase variant"
        );
        assert_eq!(
            resolve_context_window(
                "claude-opus-4-6",
                Some(NO_BETAS),
                None,
                None,
                Some("2026-03-20T00:00:00Z")
            ),
            exact(1_000_000),
            "native 1M treated as legacy"
        );
        assert_eq!(
            resolve_context_window("claude-opus-4-6", None, None, None, None),
            exact(1_000_000),
            "current capability"
        );

        // Sonnet 4.6: the same shape, two weeks later.
        assert_eq!(
            resolve_context_window(
                "claude-sonnet-4-6",
                Some(NO_BETAS),
                None,
                None,
                Some("2026-03-01T00:00:00Z")
            ),
            exact(200_000),
            "beta phase default"
        );
        assert_eq!(
            resolve_context_window(
                "claude-sonnet-4-6",
                Some(BETA),
                None,
                None,
                Some("2026-03-01T00:00:00Z")
            ),
            exact(1_000_000),
            "beta phase variant"
        );
        assert_eq!(
            resolve_context_window(
                "claude-sonnet-4-6",
                Some(NO_BETAS),
                None,
                None,
                Some("2026-03-20T00:00:00Z")
            ),
            exact(1_000_000),
            "native 1M treated as legacy"
        );

        // A widened beta cannot lift a fixed 200k model.
        assert_eq!(
            resolve_context_window("claude-haiku-4-5", Some(BETA), None, None, None),
            exact(200_000),
            "beta widened ineligible Haiku"
        );
    }

    #[test]
    fn invalid_or_unmatched_served_dates_stay_unknown_on_phased_models() {
        for at in ["not-a-date", "2026-3-1", "2026-03", "20260301T12:00:00Z"] {
            assert_eq!(
                resolve_context_window("claude-sonnet-4-0", Some(BETA), None, None, Some(at)),
                ContextWindow::Unknown,
                "{at}"
            );
        }
        // Before the first phase's `from`: no phase applied.
        assert_eq!(
            resolve_context_window(
                "claude-sonnet-4-5",
                Some(BETA),
                None,
                None,
                Some("2025-09-28T00:00:00Z")
            ),
            ContextWindow::Unknown
        );
        // A non-phased model ignores the served-at date entirely.
        assert_eq!(
            resolve_context_window("claude-opus-5", None, None, None, Some("not-a-date")),
            exact(1_000_000)
        );
    }

    #[test]
    fn context_lookup_normalises_snapshots_and_bracket_variants() {
        assert_eq!(
            resolve_context_window(" Claude-Opus-5[1m] ", None, None, None, None),
            exact(1_000_000)
        );
        assert_eq!(
            resolve_context_window(
                "claude-sonnet-4-20250514",
                Some(BETA),
                None,
                None,
                Some("2026-03-01T12:00:00Z")
            ),
            exact(1_000_000),
            "published snapshot alias folds to sonnet-4-0"
        );
        assert_eq!(
            resolve_context_window("claude-haiku-4-5-20251001", None, None, None, None),
            exact(200_000)
        );
        // An unpublished dated identity is not a published snapshot: it must
        // not become a known dateless model.
        assert_eq!(
            resolve_context_window("claude-opus-5-20990101", None, None, None, None),
            ContextWindow::Unknown
        );
    }

    #[test]
    fn published_legacy_snapshots_keep_their_fixed_200k_limit() {
        let snapshots = [
            "claude-opus-4-5-20251101",
            "claude-opus-4-1-20250805",
            "claude-opus-4-20250514",
            "claude-haiku-4-5-20251001",
            "claude-3-7-sonnet-20250219",
            "claude-3-5-sonnet-20240620",
            "claude-3-5-sonnet-20241022",
            "claude-3-5-haiku-20241022",
            "claude-3-opus-20240229",
            "claude-3-sonnet-20240229",
            "claude-3-haiku-20240307",
        ];
        for model in snapshots {
            assert_eq!(
                resolve_context_window(model, None, None, None, None),
                exact(200_000),
                "{model}"
            );
        }
    }

    #[test]
    fn unknown_claude_identities_stay_unknown_despite_captured_betas() {
        for betas in [None, Some(NO_BETAS), Some(BETA)] {
            assert_eq!(
                resolve_context_window("claude-opus-6", betas, None, None, None),
                ContextWindow::Unknown
            );
        }
    }

    #[test]
    fn codex_declarations_report_the_maximum_as_a_declared_ceiling() {
        for model in ["gpt-5.6-sol", "gpt-5.6-luna"] {
            assert_eq!(
                resolve_context_window(model, None, None, None, None),
                declared(872_000),
                "{model}"
            );
        }
        // An Anthropic-shaped beta from a compatibility gateway never makes
        // a non-Claude response 1M.
        assert_eq!(
            resolve_context_window("gpt-5.6-sol", Some(BETA), None, None, None),
            declared(872_000)
        );
        // Nor does it invent a window for an uncatalogued model.
        assert_eq!(
            resolve_context_window("other-provider-1", Some(BETA), None, None, None),
            ContextWindow::Unknown
        );
    }

    #[test]
    fn stored_declarations_resolve_to_their_model_ceiling() {
        let declaration = json!({"default": 64_000, "max": 256_000});
        assert_eq!(
            resolve_context_window("generic", None, None, Some(&declaration), None),
            declared(256_000),
            "generic stored declaration"
        );
        let claude_declared = json!({"default": 200_000, "max": 1_000_000});
        assert_eq!(
            resolve_context_window("claude-opus-6", None, None, Some(&claude_declared), None),
            declared(1_000_000),
            "unknown Claude identity with stored generic declaration"
        );
    }

    #[test]
    fn invalid_stored_declarations_are_rejected_not_repaired() {
        let invalid = [
            json!({"default": 32_000}),                   // no max
            json!({"max": 128_000}),                      // no default
            json!({"default": 32_000.5, "max": 128_000}), // fractional
            json!({"default": 128_000, "max": 32_000}),   // reversed
            json!({"default": 0, "max": 128_000}),        // zero default
            json!({"default": -1, "max": 128_000}),       // negative
            json!("200000"),                              // not an object
            json!({"default": "32000", "max": "128000"}), // string numbers
        ];
        for value in invalid {
            assert_eq!(
                resolve_context_window("generic", None, None, Some(&value), None),
                ContextWindow::Unknown,
                "{value}"
            );
            assert_eq!(DeclaredWindow::from_json(&value), None, "{value}");
        }
    }

    #[test]
    fn the_catalogue_wins_over_a_stored_declaration() {
        let stale = json!({"default": 272_000, "max": 872_000});
        assert_eq!(
            resolve_context_window("claude-opus-5", None, None, Some(&stale), None),
            exact(1_000_000),
            "catalogued model ignores a stale stored declaration"
        );
        assert_eq!(
            resolve_context_window("gpt-5.6-sol", None, None, Some(&stale), None),
            declared(872_000)
        );
    }

    /// The fetched-catalogue source (the models APIs): second to the
    /// hand-verified catalogue, above a stored declaration, above
    /// unknown — and the chain's guards hold at every step.
    #[test]
    fn fetched_listings_fill_only_the_catalogue_gaps() {
        // Hand-verified verdicts stand, whatever a listing claims —
        // including a listing that answers for a claude-named model a
        // window the catalogue disproves.
        assert_eq!(
            resolve_context_window("claude-opus-5", None, Some(123_456), None, None),
            exact(1_000_000),
            "the hand-verified catalogue encodes knowledge a listing cannot"
        );
        assert_eq!(
            resolve_context_window("gpt-5.6-sol", None, Some(999_999), None, None),
            declared(872_000)
        );
        // ...including the catalogue's own Unknown verdicts: an
        // uncaptured beta phase is a decision, not a gap.
        assert_eq!(
            resolve_context_window(
                "claude-sonnet-4-0",
                None,
                Some(1_000_000),
                None,
                Some("2026-03-01T00:00:00Z")
            ),
            ContextWindow::Unknown,
            "a fetched window must not fill an uncaptured beta phase"
        );
        // Outside the catalogue, the fetched ceiling is a declared one:
        // the openrouter `?` becomes a real ceiling.
        assert_eq!(
            resolve_context_window("z-ai/glm-5.3", None, Some(200_000), None, None),
            declared(200_000)
        );
        // It outranks a stored declaration — a fresh provider listing
        // beats a stale learned one.
        let stale = json!({"default": 32_000, "max": 64_000});
        assert_eq!(
            resolve_context_window("z-ai/glm-5.3", None, Some(200_000), Some(&stale), None),
            declared(200_000)
        );
        // With no fetched ceiling, the declaration still stands…
        assert_eq!(
            resolve_context_window("generic", None, None, Some(&stale), None),
            declared(64_000)
        );
        // …and with neither, unknown — never a guess.
        assert_eq!(
            resolve_context_window("z-ai/glm-5.3", None, None, None, None),
            ContextWindow::Unknown
        );
    }

    #[test]
    fn model_identity_folds_aliases_and_brackets_only() {
        assert_eq!(
            model_identity("Claude-Opus-5[1m]").as_deref(),
            Some("claude-opus-5")
        );
        assert_eq!(
            model_identity("claude-opus-4").as_deref(),
            Some("claude-opus-4-0")
        );
        assert_eq!(
            model_identity("claude-sonnet-4").as_deref(),
            Some("claude-sonnet-4-0")
        );
        assert_eq!(
            model_identity("claude-opus-5-20990101").as_deref(),
            Some("claude-opus-5-20990101")
        );
        assert_eq!(model_identity("").as_deref(), None);
        assert_eq!(model_identity("  ").as_deref(), None);
        // Unlike pricing's normalisation, an unpublished date stays.
        assert_eq!(
            model_identity("gpt-5.6-sol").as_deref(),
            Some("gpt-5.6-sol")
        );
    }

    #[test]
    fn context_window_reports_the_kind_strings() {
        assert_eq!(exact(1).kind(), "exact");
        assert_eq!(declared(1).kind(), "declared");
        assert_eq!(ContextWindow::Unknown.kind(), "unknown");
        assert_eq!(exact(1).tokens(), Some(1));
        assert_eq!(declared(1).tokens(), Some(1));
        assert_eq!(ContextWindow::Unknown.tokens(), None);
    }
}
