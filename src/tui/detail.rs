//! The session detail popup: one session's fuller details and its quota
//! gate controls, opened by clicking the session in the SESSIONS or
//! CONTEXT panel.
//!
//! [`Detail`] is the data, read from the store when the popup opens and
//! again on every display read while it stays open. [`lines`] and
//! [`availability`] are pure over it, so the content and which controls
//! may act are testable without a terminal; [`render`] draws it over the
//! frame like the legend, and reports where its controls landed for the
//! click handler. The writes the controls make are
//! [`crate::release`]'s, the same the release markers use.

use jiff::tz::TimeZone;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use serde_json::Value;

use super::locale::Fmt;
use super::model::SessionAgg;
use crate::catalog::windows::ContextWindow;
use crate::ir::Release;
use crate::middleware::quota::{Meters, grant_ahead};
use crate::release::GATED_BACKEND;
use crate::store::{Allowance, RequestRow, SessionSummary, Store};

/// One session's detail, as of its last read.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Detail {
    /// The session id.
    pub session: String,
    /// The session's row in the dashboard snapshot; `None` once it has
    /// aged out of the display window while the popup stayed open.
    pub agg: Option<SessionAgg>,
    /// Whole-session totals; `None` when the read failed.
    pub summary: Option<SessionSummary>,
    /// The session's newest API measurement, whole.
    pub latest: Option<RequestRow>,
    /// The allowances the session holds, live or not.
    pub allowances: Vec<Allowance>,
    /// The gated backend's last meter snapshot.
    pub meters: Option<Value>,
    /// A read that failed, said in the popup rather than tearing the
    /// dashboard down.
    pub read_error: Option<String>,
}

impl Detail {
    /// Read `session`'s detail. `agg` is its row in the current
    /// snapshot, when it has one. Store errors land in
    /// [`Detail::read_error`]; whatever did read is kept.
    pub(crate) fn load(store: &Store, session: &str, agg: Option<&SessionAgg>) -> Detail {
        let mut errors = Vec::new();
        let mut keep = |what: &str, error: crate::store::Error| {
            errors.push(format!("{what}: {error}"));
        };
        let summary = store
            .session_summary(session)
            .map_err(|error| keep("totals", error))
            .ok();
        let latest = store
            .latest_session_row(session)
            .map_err(|error| keep("latest request", error))
            .ok()
            .flatten();
        let allowances = store
            .load_session_allowances(session)
            .map_err(|error| keep("allowances", error))
            .unwrap_or_default();
        let meters = store
            .load_meters(GATED_BACKEND)
            .map_err(|error| keep("meters", error))
            .ok()
            .flatten()
            .map(|meters| meters.snapshot);
        Detail {
            session: session.to_owned(),
            agg: agg.cloned(),
            summary,
            latest,
            allowances,
            meters,
            read_error: (!errors.is_empty()).then(|| errors.join("; ")),
        }
    }

    /// The backend the session's requests went to: the snapshot's
    /// newest, else the newest measurement's.
    fn backend(&self) -> Option<&str> {
        self.agg
            .as_ref()
            .and_then(|agg| agg.provider.as_deref())
            .or_else(|| self.latest.as_ref().and_then(|row| row.provider.as_deref()))
    }

    /// The allowances whose window has not ended.
    fn live_allowances(&self, now_ms: i64) -> impl Iterator<Item = &Allowance> {
        self.allowances
            .iter()
            .filter(move |allowance| allowance.reset_value.saturating_mul(1000) > now_ms)
    }
}

/// The popup's controls, in the order they render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Control {
    /// Release to the end of the plan.
    Over,
    /// Release into overage; two-step.
    Burn,
    /// Delete the session's allowances.
    Close,
    /// Close the popup.
    Dismiss,
}

impl Control {
    pub(crate) const ALL: [Control; 4] = [
        Control::Over,
        Control::Burn,
        Control::Close,
        Control::Dismiss,
    ];

    /// The key that triggers it.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Control::Over => "o",
            Control::Burn => "b",
            Control::Close => "x",
            Control::Dismiss => "esc",
        }
    }

    /// What it says it does.
    fn label(self, burn_armed: bool) -> &'static str {
        match self {
            Control::Over => "over",
            Control::Burn if burn_armed => "confirm burn",
            Control::Burn => "burn",
            Control::Close => "close gate",
            Control::Dismiss => "dismiss",
        }
    }

    /// The release a grant control makes.
    pub(crate) fn release(self) -> Option<Release> {
        match self {
            Control::Over => Some(Release::Plan),
            Control::Burn => Some(Release::Overage),
            Control::Close | Control::Dismiss => None,
        }
    }
}

/// Why `control` cannot act right now, or `None` when it can. The grant
/// controls need the gate armed, the session on the gated backend, and a
/// meter reading naming a window to grant for; Close needs something
/// held. Dismiss always can.
pub(crate) fn availability(
    detail: &Detail,
    control: Control,
    gate_armed: bool,
    now_ms: i64,
) -> Option<&'static str> {
    if control == Control::Dismiss {
        return None;
    }
    if !gate_armed {
        return Some("gate off");
    }
    if detail.backend() != Some(GATED_BACKEND) {
        return Some("not on the subscription");
    }
    match control {
        Control::Close => detail
            .live_allowances(now_ms)
            .next()
            .is_none()
            .then_some("nothing held"),
        _ => {
            let grant = grant_ahead(detail.meters.as_ref().map(Meters::over), now_ms);
            (grant.five_hour.is_none() && grant.seven_day.is_none())
                .then_some("no meter reading yet")
        }
    }
}

/// The popup's content above the controls: who the session is, which
/// models it asked for and got, its context, its activity, and its gate.
/// Anything the store does not know is said to be unknown, never zero.
pub(crate) fn lines(
    detail: &Detail,
    gate_armed: bool,
    now_ms: i64,
    tz: &TimeZone,
    fmt: &Fmt,
) -> Vec<Line<'static>> {
    let dim = Style::new().dim();
    let field = |name: &str, value: Vec<Span<'static>>| {
        let mut spans = vec![Span::styled(format!("{name:<8}"), dim)];
        spans.extend(value);
        Line::from(spans)
    };
    let text = |value: Option<&str>| match value {
        Some(value) => Span::raw(value.to_owned()),
        None => Span::styled("unknown", dim),
    };
    let label = detail.agg.as_ref().and_then(|agg| agg.label.as_ref());
    let latest = detail.latest.as_ref();

    let mut out = Vec::new();
    let title = label.and_then(|label| label.title.as_deref().or(label.prompt.as_deref()));
    out.push(match title {
        Some(title) => Line::from(Span::raw(title.to_owned()).bold()),
        None => Line::styled("untitled", dim),
    });
    out.push(field("id", vec![Span::raw(detail.session.clone())]));
    out.push(field(
        "cwd",
        vec![text(label.and_then(|label| label.cwd.as_deref()))],
    ));
    let frontend = latest
        .and_then(|row| row.extra.as_ref())
        .and_then(|extra| extra.get("frontend"))
        .and_then(Value::as_str);
    out.push(field(
        "via",
        vec![
            text(frontend),
            Span::styled(" → ", dim),
            text(detail.backend()),
        ],
    ));

    // Models: what the client asked for, what toker sent, and what
    // answered, then each rewrite that moved it.
    let model = |value: Option<&String>| text(value.map(String::as_str));
    out.push(field(
        "model",
        vec![
            Span::styled("asked ", dim),
            model(latest.and_then(|row| row.requested_model.as_ref())),
            Span::styled(", sent ", dim),
            model(latest.and_then(|row| row.effective_model.as_ref())),
            Span::styled(", served ", dim),
            model(latest.and_then(|row| row.model.as_ref())),
        ],
    ));
    for (name, from, to) in [
        (
            "forced",
            latest.and_then(|row| row.forced_from.as_ref()),
            latest.and_then(|row| row.forced_to.as_ref()),
        ),
        (
            "downgr.",
            latest.and_then(|row| row.downgraded_from.as_ref()),
            latest.and_then(|row| row.downgraded_to.as_ref()),
        ),
    ] {
        if from.is_some() || to.is_some() {
            out.push(field(
                name,
                vec![model(from), Span::styled(" → ", dim), model(to)],
            ));
        }
    }
    if let Some(mappings) = latest.and_then(|row| row.model_mappings.as_ref()) {
        out.push(field("mapped", vec![Span::raw(mappings.to_string())]));
    }

    if let Some(agg) = &detail.agg {
        let ceiling = match agg.ctx {
            ContextWindow::Exact { tokens } => Some((tokens, "hand-verified")),
            ContextWindow::Declared { tokens } => Some((tokens, "provider listing")),
            ContextWindow::Unknown => None,
        };
        let mut spans = vec![Span::raw(fmt.grouped(agg.input_now))];
        match ceiling {
            Some((tokens, source)) => {
                spans.push(Span::raw(format!(" / {}", fmt.count(tokens as i64))));
                if let Some(now) = agg.input_now {
                    spans.push(Span::raw(format!(
                        " ({}%)",
                        (now as f64 / tokens as f64 * 100.0).round() as i64
                    )));
                }
                spans.push(Span::styled(format!(", {source}"), dim));
            }
            None => spans.push(Span::styled(" / unknown ceiling", dim)),
        }
        spans.push(Span::styled("; peak ", dim));
        spans.push(Span::raw(fmt.grouped(agg.input_peak)));
        out.push(field("context", spans));
        out.push(field(
            "",
            vec![
                Span::raw(fmt.grouped(agg.req_messages)),
                Span::styled(" messages, ", dim),
                Span::raw(fmt.grouped(agg.compact_generations)),
                Span::styled(" compactions", dim),
            ],
        ));
        out.push(field(
            "window",
            vec![
                Span::raw(fmt.count(agg.requests as i64)),
                Span::styled(" requests, output ", dim),
                Span::raw(fmt.grouped(agg.output_total)),
            ],
        ));
    } else {
        out.push(field(
            "window",
            vec![Span::styled("no requests in the display window", dim)],
        ));
    }

    match &detail.summary {
        Some(summary) if summary.requests > 0 => {
            let mut spans = vec![
                Span::raw(fmt.count(summary.requests)),
                Span::styled(" requests", dim),
            ];
            if let Some(first) = summary.first_ts_ms {
                spans.push(Span::styled(" since ", dim));
                spans.push(Span::raw(fmt.reset_label(first as f64, now_ms, tz)));
            }
            if let Some(last) = summary.last_ts_ms {
                spans.push(Span::styled(", last ", dim));
                spans.push(Span::raw(super::view::rel_age(now_ms - last)));
            }
            out.push(field("session", spans));
            out.push(field(
                "",
                vec![
                    Span::styled("in ", dim),
                    Span::raw(fmt.grouped(summary.input)),
                    Span::styled(", cache read ", dim),
                    Span::raw(fmt.grouped(summary.cache_read)),
                    Span::styled(", cache write ", dim),
                    Span::raw(fmt.grouped(summary.cache_write_total)),
                    Span::styled(", out ", dim),
                    Span::raw(fmt.grouped(summary.output)),
                ],
            ));
            if let Some(billed) = summary.billed_total {
                out.push(field("billed", vec![Span::raw(format!("${billed:.4}"))]));
            }
        }
        Some(_) => out.push(field("session", vec![Span::styled("no measurements", dim)])),
        None => out.push(field("session", vec![Span::styled("unknown", dim)])),
    }

    // The gate: armed or not, the meters it reads, and what this
    // session holds against them.
    let meters = detail.meters.as_ref().map(Meters::over);
    let mut gate = vec![if gate_armed {
        Span::raw("armed")
    } else {
        Span::styled("off", dim)
    }];
    match meters {
        Some(meters) => {
            for (name, util, reset) in [
                ("5h", meters.util5h(), meters.reset5h()),
                ("7d", meters.util7d(), meters.reset7d()),
            ] {
                gate.push(Span::styled(format!("; {name} "), dim));
                gate.push(Span::raw(match util {
                    Some(util) => format!("{}%", (util * 100.0).round() as i64),
                    None => "?".to_owned(),
                }));
                if let Some(reset) = reset {
                    gate.push(Span::styled(" resets ", dim));
                    gate.push(Span::raw(fmt.reset_label(
                        (reset * 1000) as f64,
                        now_ms,
                        tz,
                    )));
                }
            }
            if meters.overage_in_use() {
                gate.push(Span::styled("; on overage", Style::new().fg(Color::Yellow)));
            }
        }
        None => gate.push(Span::styled("; no meter reading", dim)),
    }
    out.push(field("gate", gate));
    let mut held: Vec<Span<'static>> = Vec::new();
    for allowance in detail.live_allowances(now_ms) {
        if !held.is_empty() {
            held.push(Span::styled(", ", dim));
        }
        let (what, colour) = match allowance.release {
            Release::Overage => ("overage", Color::Yellow),
            Release::Plan => ("plan", Color::Green),
        };
        held.push(Span::styled(what, Style::new().fg(colour)));
        held.push(Span::styled(
            format!(
                " for {} until {}",
                allowance.meter,
                fmt.reset_label((allowance.reset_value * 1000) as f64, now_ms, tz)
            ),
            Style::new(),
        ));
    }
    if held.is_empty() {
        held.push(Span::styled("nothing", dim));
    }
    out.push(field("held", held));
    out
}

/// Where the popup and its controls landed, for the click handler.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DrawnPopup {
    /// The popup's whole area: a click outside it closes it.
    pub area: Rect,
    /// Each control's rectangle.
    pub controls: Vec<(Rect, Control)>,
}

impl DrawnPopup {
    /// The control under `at`, if any.
    pub(crate) fn control_at(&self, at: ratatui::layout::Position) -> Option<Control> {
        self.controls
            .iter()
            .find(|(area, _)| area.contains(at))
            .map(|(_, control)| *control)
    }
}

/// Draw the popup over the frame: the content, then the controls in a
/// row (a control that cannot act shows dim with its reason), then the
/// last action's message. Sized to its content within the frame, and
/// clipped on a small terminal rather than panicking.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render(
    frame: &mut Frame,
    detail: &Detail,
    gate_armed: bool,
    burn_armed: bool,
    message: Option<&str>,
    now_ms: i64,
    tz: &TimeZone,
    fmt: &Fmt,
) -> DrawnPopup {
    let area = frame.area();
    let mut content = lines(detail, gate_armed, now_ms, tz, fmt);

    // The controls row: each `[key] label`, or `[key] reason` dim.
    let mut controls_line = Vec::new();
    let mut spans_at = Vec::new();
    let mut x = 0u16;
    for control in Control::ALL {
        let reason = availability(detail, control, gate_armed, now_ms);
        let text = format!(
            "[{}] {}",
            control.key(),
            reason.unwrap_or(control.label(burn_armed))
        );
        let style = match (reason, control) {
            (Some(_), _) => Style::new().dim(),
            (None, Control::Burn) if burn_armed => Style::new().fg(Color::Black).bg(Color::Yellow),
            (None, Control::Burn) => Style::new().fg(Color::Yellow),
            (None, Control::Over) => Style::new().fg(Color::Green),
            (None, _) => Style::new(),
        };
        let w = text.chars().count() as u16;
        if !controls_line.is_empty() {
            controls_line.push(Span::raw("   "));
            x += 3;
        }
        spans_at.push((x, w, control));
        controls_line.push(Span::styled(text, style));
        x += w;
    }
    // Wide enough for the controls on one row, so their rectangles hold;
    // the content wraps within that.
    let width = content
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or(0)
        .clamp(60, 100)
        .max(x as usize) as u16;
    let width = (width + 4).min(area.width);
    let inner_w = width.saturating_sub(4).max(1);
    content.push(Line::raw(""));
    content.push(Line::from(controls_line));
    if let Some(message) = message.or(detail.read_error.as_deref()) {
        content.push(Line::styled(
            message.to_owned(),
            Style::new().fg(Color::Red),
        ));
    }

    // Wrapped rows: the title or a long field may take more than one.
    let rows: u16 = content
        .iter()
        .map(|line| (line.width() as u16).div_ceil(inner_w).max(1))
        .sum();
    let height = (rows + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title_top("SESSION")
        .padding(ratatui::widgets::Padding::horizontal(1));
    frame.render_widget(
        Paragraph::new(content.clone())
            .block(block)
            .wrap(Wrap { trim: false }),
        popup,
    );

    // The controls row sits at the content's wrapped offset of its
    // line; when the popup was clipped above it, it has no rectangle.
    let controls_index =
        content.len() - 1 - usize::from(message.or(detail.read_error.as_deref()).is_some());
    let controls_row: u16 = content[..controls_index]
        .iter()
        .map(|line| (line.width() as u16).div_ceil(inner_w).max(1))
        .sum();
    let inner_x = popup.x + 2;
    let y = popup.y + 1 + controls_row;
    let controls = if y < popup.bottom().saturating_sub(1) {
        spans_at
            .into_iter()
            .filter(|(at, w, _)| at + w <= inner_w)
            .map(|(at, w, control)| {
                (
                    Rect {
                        x: inner_x + at,
                        y,
                        width: w,
                        height: 1,
                    },
                    control,
                )
            })
            .collect()
    } else {
        Vec::new()
    };
    DrawnPopup {
        area: popup,
        controls,
    }
}

#[cfg(test)]
mod tests {
    use super::{Control, Detail, availability, lines, render};
    use crate::config::GatesConfig;
    use crate::ir::Release;
    use crate::store::{MetersSnapshot, Store};
    use crate::tui::locale::Fmt;
    use crate::tui::model;
    use crate::tui::testrows::{as_display_rows, bare};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;
    use std::collections::HashMap;

    /// A fixed frame time; the meter resets sit an hour and four days on.
    const NOW: i64 = 2_000_000_000_000;
    const RESET5H: i64 = 2_000_003_600;
    const RESET7D: i64 = 2_000_345_600;

    fn utc() -> jiff::tz::TimeZone {
        jiff::tz::TimeZone::get("UTC").expect("UTC")
    }

    /// A store holding one subscription session whose newest turn was
    /// forced onto a newer model, with meters, and the session's detail
    /// read from it the way the loop reads it.
    fn fixture(meters: Option<serde_json::Value>) -> (Store, Detail) {
        let store = Store::open(":memory:").expect("store");
        let mut rows = Vec::new();
        for (at, prompt) in [(NOW - 120_000, 40_000), (NOW - 60_000, 52_000)] {
            let mut row = bare(at);
            row.session_id = Some("ses-detail-0001".to_owned());
            row.provider = Some("anthropic_sub".to_owned());
            row.requested_model = Some("claude-opus-5".to_owned());
            row.effective_model = Some("claude-opus-5-5".to_owned());
            row.model = Some("claude-opus-5-5".to_owned());
            row.forced_from = Some("claude-opus-5".to_owned());
            row.forced_to = Some("claude-opus-5-5".to_owned());
            row.input = Some(prompt);
            row.cache_read = Some(0);
            row.cache_write_total = Some(0);
            row.cache_write_5m = Some(0);
            row.cache_write_1h = Some(0);
            row.output = Some(900);
            row.req_messages = Some(12);
            row.extra = Some(json!({"frontend": "claude"}));
            store.record_request(&row).expect("record");
            rows.push(row);
        }
        if let Some(meters) = meters {
            store
                .save_meters(
                    "anthropic_sub",
                    &MetersSnapshot {
                        updated_ms: NOW,
                        snapshot: meters,
                    },
                )
                .expect("meters");
        }
        let snapshot = model::aggregate(
            &as_display_rows(&rows),
            None,
            &model::Released::new(),
            &HashMap::new(),
            &Default::default(),
            None,
            30,
            NOW,
            2,
        );
        let detail = Detail::load(&store, "ses-detail-0001", snapshot.sessions.first());
        (store, detail)
    }

    fn meters(util5h: f64) -> serde_json::Value {
        json!({
            "util5h": util5h, "reset5h": RESET5H,
            "util7d": 0.31, "reset7d": RESET7D,
            "overageInUse": false,
        })
    }

    /// Each control says why it cannot act, and acts once it can.
    #[test]
    fn controls_say_why_they_cannot_act() {
        let (_, detail) = fixture(None);
        assert_eq!(
            availability(&detail, Control::Over, false, NOW),
            Some("gate off")
        );
        assert_eq!(
            availability(&detail, Control::Over, true, NOW),
            Some("no meter reading yet")
        );
        assert_eq!(
            availability(&detail, Control::Close, true, NOW),
            Some("nothing held")
        );
        assert_eq!(availability(&detail, Control::Dismiss, false, NOW), None);

        let (store, detail) = fixture(Some(meters(0.40)));
        assert_eq!(availability(&detail, Control::Over, true, NOW), None);
        assert_eq!(availability(&detail, Control::Burn, true, NOW), None);
        crate::release::grant_from_tui(
            &store,
            "ses-detail-0001",
            Release::Plan,
            &GatesConfig::default(),
            NOW,
        )
        .expect("grant");
        let held = Detail::load(&store, "ses-detail-0001", detail.agg.as_ref());
        assert_eq!(availability(&held, Control::Close, true, NOW), None);
        // Once the window it covers has ended, nothing is held.
        assert_eq!(
            availability(&held, Control::Close, true, RESET5H * 1000),
            Some("nothing held")
        );

        let mut elsewhere = detail.clone();
        if let Some(agg) = &mut elsewhere.agg {
            agg.provider = Some("openrouter".to_owned());
        }
        assert_eq!(
            availability(&elsewhere, Control::Over, true, NOW),
            Some("not on the subscription")
        );
    }

    /// The content names the session in full, the models asked for, sent
    /// and served with the forcing that moved them, and the gate with
    /// what the session holds.
    #[test]
    fn the_content_names_the_session_its_models_and_its_gate() {
        let (store, _) = fixture(Some(meters(0.995)));
        crate::release::grant_from_tui(
            &store,
            "ses-detail-0001",
            Release::Overage,
            &GatesConfig::default(),
            NOW,
        )
        .expect("grant");
        let detail = Detail::load(&store, "ses-detail-0001", None);
        let text: Vec<String> = lines(&detail, true, NOW, &utc(), &Fmt::fixed())
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        let all = text.join("\n");
        assert!(all.contains("ses-detail-0001"), "{all}");
        assert!(all.contains("via     claude → anthropic_sub"), "{all}");
        assert!(
            all.contains("asked claude-opus-5, sent claude-opus-5-5, served claude-opus-5-5"),
            "{all}"
        );
        assert!(
            all.contains("forced  claude-opus-5 → claude-opus-5-5"),
            "{all}"
        );
        assert!(all.contains("2 requests"), "{all}");
        assert!(
            all.contains("in 92,000, cache read 0, cache write 0, out 1,800"),
            "{all}"
        );
        assert!(all.contains("armed; 5h 100%"), "{all}");
        assert!(all.contains("overage for 5h until"), "{all}");
        // Aged out of the window: said so, not zeroed.
        assert!(all.contains("no requests in the display window"), "{all}");
        // No title: said so.
        assert!(text[0] == "untitled", "{all}");
    }

    /// The popup draws over the frame with its controls on one row, and
    /// reports a rectangle for each that lands on its own text.
    #[test]
    fn the_popup_reports_where_each_control_landed() {
        let (_, detail) = fixture(Some(meters(0.40)));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        let mut drawn = super::DrawnPopup::default();
        terminal
            .draw(|frame| {
                drawn = render(
                    frame,
                    &detail,
                    true,
                    false,
                    None,
                    NOW,
                    &utc(),
                    &Fmt::fixed(),
                );
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let text_at = |area: ratatui::layout::Rect| -> String {
            (area.x..area.right())
                .map(|x| buffer[(x, area.y)].symbol())
                .collect()
        };
        assert_eq!(drawn.controls.len(), Control::ALL.len());
        for (area, control) in &drawn.controls {
            assert!(drawn.area.contains(area.as_position()));
            assert!(
                text_at(*area).starts_with(&format!("[{}]", control.key())),
                "{control:?}: {:?}",
                text_at(*area)
            );
            assert_eq!(drawn.control_at(area.as_position()), Some(*control));
        }
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        insta::assert_snapshot!("session_popup_100x30", text);

        // Armed, the burn control asks for its confirmation; a message
        // shows under the controls.
        terminal
            .draw(|frame| {
                drawn = render(
                    frame,
                    &detail,
                    true,
                    true,
                    Some("press burn again"),
                    NOW,
                    &utc(),
                    &Fmt::fixed(),
                );
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let burn = drawn
            .controls
            .iter()
            .find(|(_, control)| *control == Control::Burn)
            .expect("burn");
        let row: String = (burn.0.x..burn.0.right())
            .map(|x| buffer[(x, burn.0.y)].symbol())
            .collect();
        assert_eq!(row, "[b] confirm burn");

        // A tiny terminal clips without panicking.
        let mut tiny = Terminal::new(TestBackend::new(20, 6)).expect("terminal");
        tiny.draw(|frame| {
            render(
                frame,
                &detail,
                true,
                false,
                None,
                NOW,
                &utc(),
                &Fmt::fixed(),
            );
        })
        .expect("draw");
    }
}
