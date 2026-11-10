//! What the routing page calls things: a reach, a hit count, a matcher, an
//! edge, a setting. One vocabulary for the tables, the details and the
//! route page, so the same fact reads the same wherever it shows.

use ratatui::{
    style::{Color, Style},
    text::Span,
};
use serde_json::Value;

use super::data::{array, text};

/// How loudly a clause reads. Colour only ever draws the eye to a problem:
/// healthy is plain, what is merely off is dim, and a refusal is red
/// (spec: UTUI#distinguishing-a-reading; it never carries a reading alone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tone {
    Plain,
    Dim,
    Warn,
    Bad,
}

impl Tone {
    pub(super) fn style(self) -> Style {
        match self {
            Tone::Plain => Style::new(),
            Tone::Dim => Style::new().dim(),
            Tone::Warn => Style::new().fg(Color::Yellow),
            Tone::Bad => Style::new().fg(Color::Red),
        }
    }
}

pub(super) fn dim() -> Style {
    Style::new().dim()
}

pub(super) fn bold() -> Style {
    Style::new().bold()
}

/// A span in a tone.
pub(super) fn toned(content: impl Into<String>, tone: Tone) -> Span<'static> {
    Span::styled(content.into(), tone.style())
}

/// A dim label, for the left of a label and value row.
pub(super) fn label(content: impl Into<String>) -> Span<'static> {
    Span::styled(content.into(), dim())
}

/// The reach of a record as the clauses it reads in, each with its tone.
/// `ok` where the route's first candidate serves; otherwise why not, and
/// what serves instead (spec: UTUI#reading-a-table).
pub(super) fn reach_clauses(reach: &Value) -> Vec<(String, Tone)> {
    match text(reach, "outcome") {
        "served_first" => vec![("ok".to_owned(), Tone::Plain)],
        "served_later" => {
            let first = array(reach, "skipped").first();
            let why = first.map_or("unavailable", |skip| reason_word(text(skip, "reason")));
            let serving = reach.get("serving").unwrap_or(&Value::Null);
            let (fallback, tone) = match serving.get("billed").and_then(Value::as_bool) {
                Some(true) => ("billed fallback", Tone::Warn),
                Some(false) => ("unbilled fallback", Tone::Plain),
                None => ("fallback, cost unknown", Tone::Warn),
            };
            let why_tone = if why == "off" { Tone::Dim } else { Tone::Warn };
            vec![(why.to_owned(), why_tone), (fallback.to_owned(), tone)]
        }
        "refused_edge" => vec![
            ("refused".to_owned(), Tone::Bad),
            (edge_stop_word(reach).to_owned(), Tone::Bad),
        ],
        // Nothing serves, and what the walk found is what to read: `off` where
        // the providers are off, `no cred` where they lack a credential. The
        // sentence beside it says that every candidate is unavailable.
        "refused_unavailable" => {
            let mut clauses: Vec<(String, Tone)> = Vec::new();
            for skip in array(reach, "skipped") {
                let word = reason_word(text(skip, "reason"));
                if !clauses.iter().any(|(seen, _)| seen == word) {
                    let tone = if word == "off" { Tone::Dim } else { Tone::Bad };
                    clauses.push((word.to_owned(), tone));
                }
            }
            if clauses.is_empty() {
                clauses.push(("none available".to_owned(), Tone::Bad));
            }
            clauses
        }
        "refused_no_route" => {
            let cause = match text(reach, "cause") {
                "route_missing" => "route missing",
                _ => "no route",
            };
            vec![
                ("refused".to_owned(), Tone::Warn),
                (cause.to_owned(), Tone::Warn),
            ]
        }
        _ => vec![("-".to_owned(), Tone::Dim)],
    }
}

/// A clause in its fewest words, for where the room is short: what is billed
/// is billed whichever way it came to be served.
fn briefly(clause: &str) -> &str {
    match clause {
        "billed fallback" => "billed",
        "unbilled fallback" => "unbilled",
        "fallback, cost unknown" => "cost unknown",
        other => other,
    }
}

/// The reach as spans, its clauses a `·` apart.
pub(super) fn reach_spans(reach: &Value) -> Vec<Span<'static>> {
    reach_spans_in(reach, false)
}

/// The reach as spans, in its fewest words where `brief`.
pub(super) fn reach_spans_in(reach: &Value, brief: bool) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, (clause, tone)) in reach_clauses(reach).into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" · "));
        }
        let clause = if brief {
            briefly(&clause).to_owned()
        } else {
            clause
        };
        spans.push(toned(clause, tone));
    }
    spans
}

/// The reach as text, in its fewest words where `brief`.
pub(super) fn reach_text_in(reach: &Value, brief: bool) -> String {
    reach_clauses(reach)
        .into_iter()
        .map(|(clause, _)| {
            if brief {
                briefly(&clause).to_owned()
            } else {
                clause
            }
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A reach where the record has none to show: a presentation that is off, or
/// a profile with no default of its own.
pub(super) fn no_reach() -> Vec<Span<'static>> {
    vec![toned("-", Tone::Dim)]
}

fn edge_stop_word(reach: &Value) -> &'static str {
    match reach
        .get("edge")
        .and_then(|edge| edge.get("setting"))
        .and_then(Value::as_str)
    {
        Some("billed_boundary") => "billed boundary denied",
        _ => "fallback disabled",
    }
}

/// A skip reason in a word or two, for a table cell.
pub(super) fn reason_word(code: &str) -> &'static str {
    match code {
        "candidate_disabled" => "disabled",
        "binding_unknown" => "unknown binding",
        "provider_unconfigured" => "not configured",
        "provider_off" => "off",
        "no_credential" => "no cred",
        "missing_capability" => "no capability",
        "lacks_server_safeguards" => "no safeguards",
        "binding_affinity" => "other binding",
        "model_not_offered" => "not offered",
        "model_unknown" => "unknown model",
        "selector_unresolved" => "no model",
        "health_circuit_open" => "circuit open",
        "fallback_disabled" => "no fallback",
        "billed_boundary_denied" => "billed denied",
        "warm_cache_transition_denied" => "cache kept",
        "warm_lane_transition_denied" => "lane kept",
        _ => "unavailable",
    }
}

/// What a skip reason says of the provider it skipped, for a sentence.
fn reason_phrase(code: &str) -> &'static str {
    match code {
        "candidate_disabled" => "is switched off in the route",
        "binding_unknown" => "has no such binding",
        "provider_unconfigured" => "is not configured",
        "provider_off" => "is off",
        "no_credential" => "has no credential",
        "missing_capability" => "lacks a capability the request needs",
        "lacks_server_safeguards" => "lacks server safeguards",
        "binding_affinity" => "is not the binding the lane is on",
        "model_not_offered" => "does not offer the model",
        "model_unknown" => "does not know the model",
        "selector_unresolved" => "offers no model the selector picks",
        "health_circuit_open" => "has an open health circuit",
        "fallback_disabled" => "may not be fallen back from",
        "billed_boundary_denied" => "would cross to billed, which is denied",
        "warm_cache_transition_denied" => "would lose a warm cache",
        "warm_lane_transition_denied" => "would move a warm lane",
        _ => "is unavailable",
    }
}

/// Why a reach is what it is, as a sentence: `anthropic_sub has no
/// credential, so openrouter serves it, billed`.
pub(super) fn reach_reason(reach: &Value) -> String {
    let serving = reach.get("serving").unwrap_or(&Value::Null);
    let skipped = array(reach, "skipped");
    let passed = |skip: &Value| {
        let who = match text(skip, "reason") {
            "candidate_disabled" => text(skip, "candidate_id"),
            _ => text(skip, "provider"),
        };
        format!("{who} {}", reason_phrase(text(skip, "reason")))
    };
    let billed = match serving.get("billed").and_then(Value::as_bool) {
        Some(true) => ", billed",
        Some(false) => ", not billed",
        None => ", cost unknown",
    };
    match text(reach, "outcome") {
        // Nothing to explain: the first choice serves it.
        "served_first" => String::new(),
        "served_later" => format!(
            "{}, so {} serves it{billed}",
            skipped.iter().map(passed).collect::<Vec<_>>().join("; "),
            text(serving, "provider"),
        ),
        "refused_edge" => {
            let edge = reach.get("edge").unwrap_or(&Value::Null);
            let setting = match text(edge, "setting") {
                "billed_boundary" => "the billed boundary is denied",
                _ => "falling back before a send is disabled",
            };
            let before = if skipped.is_empty() {
                String::new()
            } else {
                format!(
                    "{}; ",
                    skipped.iter().map(passed).collect::<Vec<_>>().join("; ")
                )
            };
            format!(
                "{before}the walk stops between {} and {}: {setting}",
                text(edge, "from"),
                text(edge, "to"),
            )
        }
        "refused_unavailable" => format!(
            "every candidate is unavailable: {}",
            skipped.iter().map(passed).collect::<Vec<_>>().join("; ")
        ),
        "refused_no_route" => match text(reach, "cause") {
            "route_missing" => "the route it names is not in the policy".to_owned(),
            _ => "no default catches what no rule did, so the request is refused".to_owned(),
        },
        _ => String::new(),
    }
}

/// The hit count of a record as a table cell: a figure, a floor `≥n`, or a
/// dim dash where it was not recorded. A floor of nothing says nothing, so
/// it is a dash too (spec: UTUI#reading-a-table).
pub(super) fn hits_span(hits: Option<&Value>) -> Span<'static> {
    match hits_figure(hits) {
        Some(figure) => Span::raw(figure),
        None => toned("-", Tone::Dim),
    }
}

fn hits_figure(hits: Option<&Value>) -> Option<String> {
    let hits = hits?;
    let count = hits.get("count").and_then(Value::as_u64)?;
    match text(hits, "state") {
        "floor" if count == 0 => None,
        "floor" => Some(format!("≥{count}")),
        "exact" => Some(count.to_string()),
        _ => None,
    }
}

/// The same as a phrase: `≥3 in 24h`, or a dash.
pub(super) fn hits_phrase(hits: Option<&Value>) -> String {
    match hits_figure(hits) {
        Some(figure) if figure == "0" => "none in 24h".to_owned(),
        Some(figure) => format!("{figure} in 24h"),
        None => "-".to_owned(),
    }
}

/// A matcher in the notation it is written in, as the daemon spelled it: a
/// prefix with the part it captures, a glob, an exact name, and a regular
/// expression only for one.
pub(super) fn matcher_text(matcher: &Value) -> String {
    text(matcher, "text").to_owned()
}

pub(super) fn matcher_kind(matcher: &Value) -> &str {
    text(matcher, "kind")
}

/// A lane transition in words.
pub(super) fn lane_words(code: &str) -> &'static str {
    match code {
        "preserve_pinned" => "stays pinned",
        "move_when_cold" => "moves when cold",
        "allow_immediate" => "moves at once",
        _ => "?",
    }
}

/// A cache transition in words.
pub(super) fn cache_words(code: &str) -> &'static str {
    match code {
        "preserve_warm" => "preserve warm",
        "allow_when_cold" => "allowed when cold",
        "allow_cache_loss" => "loss allowed",
        _ => "?",
    }
}

/// A failure class an edge falls back on, in words.
fn failure_words(code: &str) -> &'static str {
    match code {
        "authentication" => "auth failure",
        "model_not_found" => "model not found",
        "rate_limit" => "rate limited",
        "provider_quota" => "provider quota",
        "overload" => "overloaded",
        "server_error" => "5xx",
        "transport_before_accept" => "connection failed",
        "ambiguous_transport" => "ambiguous transport",
        _ => "?",
    }
}

/// The edge's two halves: when it falls back, and what it consents to.
pub(super) fn edge_parts(edge: &Value) -> (String, String) {
    let mut when: Vec<&str> = Vec::new();
    if text(edge, "preflight") == "enabled" {
        when.push("unavailable before send");
    }
    when.extend(
        array(edge, "post_send")
            .iter()
            .filter_map(Value::as_str)
            .map(failure_words),
    );
    let when = if when.is_empty() {
        "never".to_owned()
    } else {
        format!("when {}", when.join(" · "))
    };
    (
        format!("↓ {when}"),
        format!(
            "billed: {}   ambiguous retry: {}   cache: {}",
            text(edge, "billed_boundary"),
            text(edge, "ambiguous_transport"),
            cache_words(text(edge, "cache_transition")),
        ),
    )
}

/// A cost class as the route page writes it.
pub(super) fn cost_word(cost: &str) -> &str {
    match cost {
        "subscription" | "billed" => cost,
        _ => "cost unknown",
    }
}

/// Lanes held, as `2 warm · 5 cold`; `none` where there are none.
pub(super) fn lanes_phrase(lanes: &Value) -> String {
    let mut parts = Vec::new();
    for (key, word) in [("warm", "warm"), ("cold", "cold"), ("unknown", "unknown")] {
        let count = super::data::count(lanes, key);
        if count > 0 {
            parts.push(format!("{count} {word}"));
        }
    }
    if parts.is_empty() {
        "none".to_owned()
    } else {
        parts.join(" · ")
    }
}

/// How many of each kind of record send to a route: `4 rules · 2 defaults`,
/// leaving out a kind with none.
pub(super) fn matched_by_phrase(counts: &Value) -> String {
    let mut parts = Vec::new();
    for (key, one, many) in [
        ("presentations", "presentation", "presentations"),
        ("rewrites", "rule", "rules"),
        ("defaults", "default", "defaults"),
    ] {
        let count = super::data::count(counts, key);
        if count > 0 {
            parts.push(super::data::counted(count as usize, one, many));
        }
    }
    if parts.is_empty() {
        "nothing".to_owned()
    } else {
        parts.join(" · ")
    }
}

/// A moment as the history and the recent requests write it: `Tue 10 Nov
/// 14:41`, in the operator's time zone.
pub(super) fn when(ms: i64) -> String {
    let Ok(at) = jiff::Timestamp::from_millisecond(ms) else {
        return "-".to_owned();
    };
    // A frame under test must not depend on where it is run.
    #[cfg(test)]
    let zone = jiff::tz::TimeZone::UTC;
    #[cfg(not(test))]
    let zone = jiff::tz::TimeZone::system();
    at.to_zoned(zone).strftime("%a %d %b %H:%M").to_string()
}
