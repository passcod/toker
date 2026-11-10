//! Tracing a request through the policy: an invented one, or one picked from
//! the recent requests (spec: UTUI#tracing-a-request).
//!
//! The daemon resolves it by the same preview the editor uses and answers with
//! the walk; this reads that answer as a panel, outcome first and then one
//! line per stage, and marks the tables the walk passed through. The tables
//! show the live revision, so a trace under any other revision draws its
//! stages but marks nothing.

use std::collections::BTreeMap;

use ratatui::text::{Line, Span};
use reqwest::Method;
use serde_json::{Value, json};

use super::data::{array, count, text};
use super::model::Tab;
use super::rules::{self, Entry};
use crate::tui::control::{Command, Response};
use crate::tui::table::{controls, reversed, span};
use crate::tui::tone::{Tone, bold, dim, toned};

/// What is traced.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Subject {
    /// A request described to the resolver; the body's `request` object.
    Invented(Value),
    /// A request the ledger recorded the facts of.
    Recorded(i64),
}

/// A trace, asked for and then answered.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Trace {
    pub(super) subject: Subject,
    /// The revision it is traced under, where that is not the live one.
    pub(super) under: Option<i64>,
    answer: Answer,
}

#[derive(Debug, Clone, PartialEq)]
enum Answer {
    Pending,
    Failed(String),
    Walked(Box<Value>),
}

impl Trace {
    pub(super) fn new(subject: Subject, under: Option<i64>) -> Trace {
        Trace {
            subject,
            under,
            answer: Answer::Pending,
        }
    }

    /// The preview that answers it.
    pub(super) fn command(&self) -> Command {
        let request = match &self.subject {
            Subject::Invented(request) => request.clone(),
            Subject::Recorded(id) => json!({ "kind": "recorded", "request_id": id }),
        };
        let against = match self.under {
            Some(revision_id) => json!({ "kind": "revision", "revision_id": revision_id }),
            None => json!({ "kind": "active" }),
        };
        Command {
            name: "trace",
            method: Method::POST,
            path: "preview",
            control: "routing-preview",
            body: Some(json!({
                "mode": "resolve",
                "against": against,
                "request": request,
            })),
        }
    }

    /// Take the daemon's answer, if it is to the question being asked now.
    pub(super) fn receive(&mut self, command: &Command, response: &Response, error: String) {
        if command.body != self.command().body {
            return;
        }
        self.answer = if response.status / 100 == 2 {
            Answer::Walked(Box::new(response.body.clone()))
        } else {
            Answer::Failed(error)
        };
    }

    pub(super) fn failed(&mut self, error: String) {
        self.answer = Answer::Failed(error);
    }

    fn body(&self) -> Option<&Value> {
        match &self.answer {
            Answer::Walked(body) => Some(body),
            _ => None,
        }
    }

    /// Whether the tables on the page are what this trace walked: it was made
    /// under the live revision.
    fn on_live(&self, live: Option<i64>) -> bool {
        self.under.is_none_or(|revision| Some(revision) == live)
    }

    fn result(&self) -> Option<&Value> {
        self.body().and_then(|body| body.get("result"))
    }

    /// The scope labels the request was looked up under, narrowest first.
    fn scopes(&self) -> Vec<String> {
        let Some(request) = self.body().and_then(|body| body.get("request")) else {
            return Vec::new();
        };
        let protocol = text(request, "protocol");
        let mut scopes = Vec::new();
        if let Some(profile) = request.get("profile").and_then(Value::as_str) {
            scopes.push(format!("{profile}/{protocol}"));
        }
        scopes.push(protocol.to_owned());
        scopes
    }

    fn provenance(&self) -> Option<&Value> {
        self.result()
            .and_then(|result| result.get("provenance"))
            .filter(|provenance| provenance.is_object())
    }

    fn route_id(&self) -> Option<&str> {
        self.result()
            .and_then(|result| result.get("route_id"))
            .and_then(Value::as_str)
    }

    /// Where `enter` goes: the record that caught the request.
    pub(super) fn catch(&self, active: &Value, live: Option<i64>) -> Option<(Tab, usize)> {
        if !self.on_live(live) {
            return None;
        }
        let provenance = self.provenance()?;
        // The record is found by the field that names it in its own table.
        let (tab, field, wanted) = match text(provenance, "kind") {
            "presentation" => (Tab::Presentations, "id", text(provenance, "id").to_owned()),
            "rewrite" => (Tab::Rewrites, "id", text(provenance, "id").to_owned()),
            "default" => (Tab::Defaults, "scope_label", self.default_label(provenance)),
            _ => return None,
        };
        rules::entries(active, tab)
            .iter()
            .position(|entry| {
                // A rewrites table also lists the defaults that end its groups.
                !matches!((tab, entry), (Tab::Rewrites, Entry::Default(_)))
                    && text(entry.record(), field) == wanted
            })
            .map(|at| (tab, at))
    }

    /// The scope label of the default that caught the request.
    fn default_label(&self, provenance: &Value) -> String {
        let scopes = self.scopes();
        match text(provenance, "scope") {
            "profile" => scopes.first().cloned().unwrap_or_default(),
            _ => scopes.last().cloned().unwrap_or_default(),
        }
    }

    // --- marks ------------------------------------------------------------

    /// The marks a tab's table carries for this trace, by entry: `>` on the
    /// row that caught the request, `↓` on the rows it fell past.
    pub(super) fn marks(
        &self,
        active: &Value,
        tab: Tab,
        live: Option<i64>,
    ) -> BTreeMap<usize, char> {
        let mut marks = BTreeMap::new();
        if !self.on_live(live) {
            return marks;
        }
        let Some(result) = self.result() else {
            return marks;
        };
        if tab == Tab::Routes {
            if let Some(route) = self.route_id() {
                let at = array(active, "routes")
                    .iter()
                    .position(|record| text(record, "id") == route);
                marks.extend(at.map(|at| (at, '>')));
            }
            return marks;
        }
        let scopes = self.scopes();
        // How far down the lookup the request went: 1 presentations, 2
        // rewrites, 3 defaults, 4 all of them with nothing caught.
        let (provenance, stage) = match self.provenance() {
            Some(provenance) => {
                let stage = match text(provenance, "kind") {
                    "presentation" => 1,
                    "rewrite" => 2,
                    "default" => 3,
                    _ => return marks,
                };
                (provenance, stage)
            }
            None if text(result, "outcome") == "unresolved" => (&Value::Null, 4),
            None => return marks,
        };
        let caught_scope = match text(provenance, "scope") {
            "profile" => scopes.first(),
            _ => scopes.last(),
        };
        for (at, entry) in rules::entries(active, tab).iter().enumerate() {
            let record = entry.record();
            let label = text(record, "scope_label");
            let looked_at = scopes.iter().position(|scope| scope == label);
            let Some(scope_rank) = looked_at else {
                continue;
            };
            let caught_rank = caught_scope.and_then(|scope| scopes.iter().position(|s| s == scope));
            match (tab, entry) {
                (Tab::Presentations, Entry::Presentation(_)) => {
                    if stage == 1 && text(record, "id") == text(provenance, "id") {
                        marks.insert(at, '>');
                    } else if stage > 1 && record.get("enabled") != Some(&Value::Bool(false)) {
                        marks.insert(at, '↓');
                    }
                }
                (Tab::Rewrites | Tab::Defaults, Entry::Rewrite(_)) if stage >= 2 => {
                    let position = count(record, "position");
                    if stage == 2 && text(record, "id") == text(provenance, "id") {
                        marks.insert(at, '>');
                    } else if stage >= 3
                        || Some(scope_rank) < caught_rank
                        || (Some(scope_rank) == caught_rank
                            && position < self.caught_position(active, provenance))
                    {
                        marks.insert(at, '↓');
                    }
                }
                (Tab::Rewrites | Tab::Defaults, Entry::Default(_)) if stage >= 3 => {
                    if record.get("exists") != Some(&Value::Bool(true)) {
                        continue;
                    }
                    if stage == 4 {
                        marks.insert(at, '↓');
                    } else if Some(scope_rank) == caught_rank {
                        marks.insert(at, '>');
                    } else if Some(scope_rank) < caught_rank {
                        marks.insert(at, '↓');
                    }
                }
                _ => {}
            }
        }
        marks
    }

    /// The position of the rewrite that caught the request in its scope.
    fn caught_position(&self, active: &Value, provenance: &Value) -> u64 {
        array(active, "rewrites")
            .iter()
            .find(|rule| text(rule, "id") == text(provenance, "id"))
            .map_or(0, |rule| count(rule, "position"))
    }

    // --- the panel --------------------------------------------------------

    /// The panel's lines: what was traced, where it goes, then each stage.
    pub(super) fn lines(
        &self,
        snapshot: &Value,
        live: Option<i64>,
        width: usize,
    ) -> Vec<Line<'static>> {
        let keys = controls(&[
            ("t", "trace another"),
            ("enter", "open link"),
            ("esc", "close"),
        ]);
        let mut lines = vec![self.heading()];
        let Some(body) = self.body() else {
            lines.push(match &self.answer {
                Answer::Failed(error) => Line::from(toned(error.clone(), Tone::Bad)),
                _ => Line::from(span("tracing…", dim())),
            });
            lines.push(keys);
            return lines;
        };
        let result = body.get("result").unwrap_or(&Value::Null);
        lines.push(self.outcome(snapshot, result));
        if text(result, "outcome") == "not_replayable" {
            lines.push(keys);
            return lines;
        }
        let active = super::data::active(snapshot).filter(|_| self.on_live(live));
        let mut numbered = Numbered::new(width);
        self.lookup_stages(&mut numbered, active, result);
        if let Some(route) = self.route_id() {
            numbered.plain("route", vec![Span::raw(route.to_owned())]);
        }
        self.walk_stages(&mut numbered, snapshot, result);
        lines.extend(numbered.lines);
        lines.push(keys);
        lines
    }

    fn heading(&self) -> Line<'static> {
        let what = match (
            &self.subject,
            self.body().and_then(|body| body.get("request")),
        ) {
            (_, Some(request)) if request.is_object() => {
                let profile = request
                    .get("profile")
                    .and_then(Value::as_str)
                    .unwrap_or("unprefixed");
                let mut said = format!("{profile}/{}", text(request, "protocol"));
                if let Some(model) = request.get("model").and_then(Value::as_str) {
                    said.push_str(&format!(" · model {model}"));
                }
                let prompt = count(request, "prompt_bound");
                if prompt > 0 {
                    said.push_str(&format!(" · {} tokens", thousands(prompt)));
                }
                said
            }
            (Subject::Recorded(id), _) => format!("request #{id}"),
            (Subject::Invented(request), _) => format!(
                "{}/{}",
                request
                    .get("profile")
                    .and_then(Value::as_str)
                    .unwrap_or("unprefixed"),
                text(request, "protocol")
            ),
        };
        let mut spans = vec![span("trace", bold()), Span::raw(format!("  {what}"))];
        if let Subject::Recorded(id) = &self.subject
            && self
                .body()
                .and_then(|body| body.get("request"))
                .is_some_and(Value::is_object)
        {
            spans.push(span(format!(" · recorded request #{id}"), dim()));
        }
        if let Some(revision) = self.under {
            spans.push(span(format!(" · under rev {revision}"), dim()));
        }
        Line::from(spans)
    }

    /// The answer, first: where it goes and whether that is billed, or why
    /// it goes nowhere.
    fn outcome(&self, snapshot: &Value, result: &Value) -> Line<'static> {
        match text(result, "outcome") {
            "selected" => {
                let mut spans = vec![span(
                    format!(
                        "goes to  {} as {} · ",
                        text(result, "binding_id"),
                        text(result, "effective_model")
                    ),
                    bold(),
                )];
                spans.push(match result.get("billed").and_then(Value::as_bool) {
                    Some(true) => toned("billed", Tone::Warn).patch_style(bold()),
                    Some(false) => span("not billed", bold()),
                    None => toned("cost unknown", Tone::Warn).patch_style(bold()),
                });
                let _ = snapshot;
                Line::from(spans)
            }
            "unavailable" => Line::from(toned(
                format!(
                    "refused · no candidate on {} can serve it",
                    text(result, "route_id")
                ),
                Tone::Bad,
            )),
            "unresolved" => Line::from(toned(
                match text(result, "code") {
                    "no_route" => {
                        "refused · no presentation, rewrite or default catches it".to_owned()
                    }
                    "missing_route" => {
                        "refused · the route it names is not in the policy".to_owned()
                    }
                    other => format!("cannot be resolved · {}", other.replace('_', " ")),
                },
                Tone::Bad,
            )),
            "not_replayable" => Line::from(toned(
                format!(
                    "not replayable · {}",
                    not_replayable_words(text(result, "reason"))
                ),
                Tone::Warn,
            )),
            _ => Line::from(span("no answer", dim())),
        }
    }

    /// The three lookup stages: which caught the request and which it fell
    /// past.
    fn lookup_stages(&self, out: &mut Numbered, active: Option<&Value>, result: &Value) {
        let Some(provenance) = result.get("provenance").filter(|value| value.is_object()) else {
            // Nothing caught it: every stage was passed.
            if text(result, "outcome") == "unresolved" {
                out.fell("presentations", "no slug equals it");
                out.fell("rewrites", "no rule matches it");
                out.fell("defaults", "no default");
            }
            return;
        };
        let rule = |id: &str| -> Vec<Span<'static>> {
            let known = active.and_then(|active| {
                array(active, "rewrites")
                    .iter()
                    .find(|rule| text(rule, "id") == id)
            });
            match known {
                Some(rule) => vec![Span::raw(format!(
                    "rule {} in {}  {} → {}",
                    count(rule, "position"),
                    text(rule, "scope_label"),
                    rule.get("matcher")
                        .map(super::words::matcher_text)
                        .unwrap_or_default(),
                    text(rule, "route_id"),
                ))],
                None => vec![Span::raw(format!("rule {id}"))],
            }
        };
        match text(provenance, "kind") {
            "presentation" => {
                let id = text(provenance, "id");
                let slug = active.and_then(|active| {
                    array(active, "presentation_records")
                        .iter()
                        .find(|record| text(record, "id") == id)
                        .map(|record| text(record, "slug").to_owned())
                });
                out.caught(
                    "presentations",
                    vec![Span::raw(match slug {
                        Some(slug) => format!("presentation {slug}"),
                        None => format!("presentation {id}"),
                    })],
                );
                out.not_reached("rewrites");
                out.not_reached("defaults");
            }
            "rewrite" => {
                out.fell("presentations", "no slug equals it");
                out.caught("rewrites", rule(text(provenance, "id")));
                out.not_reached("defaults");
            }
            "default" => {
                out.fell("presentations", "no slug equals it");
                out.fell("rewrites", "no rule matches it");
                let label = self.default_label(provenance);
                out.caught("defaults", vec![Span::raw(format!("default for {label}"))]);
            }
            _ => {
                out.caught(
                    "compaction",
                    vec![Span::raw("the route compactions are sent to".to_owned())],
                );
            }
        }
    }

    /// The route's candidates in the order they were tried, then what shaped
    /// the model on the one that served.
    fn walk_stages(&self, out: &mut Numbered, snapshot: &Value, result: &Value) {
        let mut at = 0;
        let mut upgrade = None;
        let mut alias = None;
        for event in array(result, "trace") {
            match text(event, "kind") {
                "candidate" => {
                    at += 1;
                    let name = format!("candidate {at}");
                    let binding = text(event, "binding_id").to_owned();
                    match event.get("skipped").and_then(Value::as_str) {
                        Some(reason) => out.fell_with(
                            &name,
                            vec![
                                Span::raw(format!("{binding}  ")),
                                toned(super::words::reason_word(reason), Tone::Warn),
                            ],
                        ),
                        None => {
                            let billed = match result.get("billed").and_then(Value::as_bool) {
                                Some(true) => toned("billed", Tone::Warn),
                                Some(false) => Span::raw("not billed"),
                                None => toned("cost unknown", Tone::Warn),
                            };
                            out.serves(&name, vec![Span::raw(format!("{binding}  ")), billed]);
                        }
                    }
                }
                "upgrade" => upgrade = Some((text(event, "from"), text(event, "to"))),
                "alias" => alias = Some((text(event, "from"), text(event, "to"))),
                _ => {}
            }
        }
        if let Some((_, to)) = alias {
            // The alias belongs to the candidate that served.
            if let Some(line) = out.lines.last_mut() {
                line.spans.push(span(format!("  alias → {to}"), dim()));
            }
        }
        if text(result, "outcome") != "selected" {
            return;
        }
        match upgrade {
            Some((from, to)) => out.plain("upgrade", vec![Span::raw(format!("{from} → {to}"))]),
            None => out.verdict("upgrade", "none", Vec::new()),
        }
        let binding = text(result, "binding_id");
        let availability = snapshot
            .get("derived")
            .map(|derived| array(derived, "availability"))
            .unwrap_or(&[])
            .iter()
            .find(|live| text(live, "binding_id") == binding);
        if let Some(live) = availability {
            let on = live
                .get("enabled")
                .map(|evidence| text(evidence, "state"))
                .unwrap_or("");
            out.verdict(
                "provider",
                if on == "no" { "off" } else { "on" },
                vec![Span::raw(text(result, "provider").to_owned())],
            );
        }
    }
}

/// A request's age-old figure with thousands separators: `6,883`.
fn thousands(number: u64) -> String {
    let digits = number.to_string();
    let mut out = String::new();
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Why a recorded request cannot be traced.
pub(super) fn not_replayable_words(code: &str) -> &'static str {
    match code {
        "facts_not_recorded" => "it was routed before the resolver's facts were recorded",
        "compaction_evidence_not_recorded" => {
            "a compaction's evidence that the lane was cold was not recorded"
        }
        _ => "its facts were not recorded",
    }
}

/// The stage lines of a trace, numbered as they are added.
struct Numbered {
    width: usize,
    lines: Vec<Line<'static>>,
}

/// The widest a stage's name and number run to: `5 candidate 1`, padded to a
/// column.
const NAME: usize = 17;
/// The widest verdict, `↓ fell past`, padded to a column.
const VERDICT: usize = 13;

impl Numbered {
    fn new(width: usize) -> Self {
        Numbered {
            width,
            lines: Vec::new(),
        }
    }

    fn line(&mut self, name: &str, verdict: Vec<Span<'static>>, rest: Vec<Span<'static>>) {
        let number = self.lines.len() + 1;
        let mut spans = vec![Span::raw(format!(
            "{:<width$}",
            format!("{number} {name}"),
            width = NAME
        ))];
        let used: usize = verdict.iter().map(Span::width).sum();
        spans.extend(verdict);
        // A line with no verdict, such as the route, runs on from its name.
        if used > 0 {
            spans.push(Span::raw(" ".repeat(VERDICT.saturating_sub(used))));
        }
        spans.extend(rest);
        self.lines.push(Line::from(spans));
    }

    fn plain(&mut self, name: &str, rest: Vec<Span<'static>>) {
        self.line(name, Vec::new(), rest);
    }

    fn verdict(&mut self, name: &str, verdict: &str, rest: Vec<Span<'static>>) {
        self.line(name, vec![Span::raw(verdict.to_owned())], rest);
    }

    fn fell(&mut self, name: &str, why: &str) {
        self.fell_with(name, vec![span(why.to_owned(), dim())]);
    }

    fn fell_with(&mut self, name: &str, rest: Vec<Span<'static>>) {
        self.line(name, vec![Span::raw("↓ fell past")], rest);
    }

    /// The stage that caught the request: its line is reversed, as the row
    /// it names is in the table.
    fn caught(&mut self, name: &str, rest: Vec<Span<'static>>) {
        self.line(name, vec![span("> caught", bold())], rest);
        let width = self.width;
        if let Some(line) = self.lines.pop() {
            self.lines.push(reversed(line, width));
        }
    }

    fn serves(&mut self, name: &str, rest: Vec<Span<'static>>) {
        self.line(name, vec![span("> serves", bold())], rest);
    }

    fn not_reached(&mut self, name: &str) {
        self.line(name, vec![span("not reached", dim())], Vec::new());
        if let Some(line) = self.lines.pop() {
            let dimmed = Line::from(
                line.spans
                    .into_iter()
                    .map(|part| span(part.content.to_string(), dim()))
                    .collect::<Vec<_>>(),
            );
            self.lines.push(dimmed);
        }
    }
}

/// The question asked before a trace: an invented request, or one of the
/// recent ones.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Picking {
    /// What the ledger lists, once it has answered.
    recent: Option<Vec<Value>>,
    /// The choice: the invented request is first, the recent ones follow.
    at: usize,
    /// The revision the chosen request is traced under.
    pub(super) under: Option<i64>,
    failed: Option<String>,
}

/// The most rows of the list shown at once.
const PICKS: usize = 10;

impl Picking {
    pub(super) fn new(under: Option<i64>) -> Picking {
        Picking {
            recent: None,
            at: 0,
            under,
            failed: None,
        }
    }

    /// The listing that fills it.
    pub(super) fn command() -> Command {
        Command {
            name: "recent",
            method: Method::POST,
            path: "preview",
            control: "routing-preview",
            body: Some(json!({ "mode": "recent_requests", "limit": 50 })),
        }
    }

    pub(super) fn receive(&mut self, response: &Response, error: String) {
        if response.status / 100 == 2 {
            self.recent = Some(array(&response.body, "requests").to_vec());
            self.failed = None;
        } else {
            self.failed = Some(error);
        }
    }

    pub(super) fn step(&mut self, delta: isize) {
        let rows = 1 + self.recent.as_ref().map_or(0, Vec::len);
        self.at = self.at.saturating_add_signed(delta).min(rows - 1);
    }

    /// The recent request chosen, or `None` for the invented one.
    pub(super) fn chosen(&self) -> Option<i64> {
        let request = self.recent.as_ref()?.get(self.at.checked_sub(1)?)?;
        request.get("id").and_then(Value::as_i64)
    }

    /// The popup's lines: the invented request, then the recent ones
    /// newest first, those that cannot be replayed said to be so.
    pub(super) fn text(&self) -> Vec<String> {
        let mut rows = vec!["an invented request…".to_owned()];
        let mut waiting = None;
        match (&self.recent, &self.failed) {
            (Some(recent), _) => rows.extend(recent.iter().map(recent_row)),
            (None, Some(error)) => waiting = Some(error.clone()),
            (None, None) => waiting = Some("reading recent requests…".to_owned()),
        }
        let start = self
            .at
            .saturating_sub(PICKS / 2)
            .min(rows.len().saturating_sub(PICKS));
        let mut lines: Vec<String> = rows
            .iter()
            .enumerate()
            .skip(start)
            .take(PICKS)
            .map(|(at, row)| format!("{} {row}", if at == self.at { "▸" } else { " " }))
            .collect();
        if rows.len() > start + PICKS {
            lines.push(format!("  … {} more", rows.len() - start - PICKS));
        }
        lines.extend(waiting.map(|note| format!("  {note}")));
        lines
    }
}

/// A recent request as a line of the list: when, what was asked for and
/// where it went.
fn recent_row(request: &Value) -> String {
    let id = request.get("id").and_then(Value::as_i64).unwrap_or(0);
    let when = super::words::when(request.get("ts_ms").and_then(Value::as_i64).unwrap_or(0));
    let asked = request
        .get("requested_model")
        .and_then(Value::as_str)
        .unwrap_or("no model");
    let went = match request
        .get("routed")
        .and_then(|routed| routed.get("binding_id"))
        .and_then(Value::as_str)
    {
        Some(binding) => format!("→ {binding}"),
        None => "→ refused".to_owned(),
    };
    let mut row = format!("{:<5}{when}  {asked}  {went}", format!("#{id}"));
    if request.get("replayable") == Some(&Value::Bool(false)) {
        row.push_str("  · not replayable");
    }
    row
}
