//! The providers page: a table of every provider Toker knows, and the
//! selected one's detail beneath it (spec: UTUI#providers).
//!
//! It draws with the routing page's own vocabulary, so the two read as one
//! family: dim for what is off, yellow only for a problem and always with a
//! word beside it, the selected row reversed, the controls on the detail's
//! last line.

use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
};

use super::form;
use super::model::{Dependents, Provider, Row, Safeguards, Snapshot, State, Target};
use crate::tui::chrome::RowHit;
use crate::tui::table::{
    self, BodyLine, Cells, Detail, Table, controls_in, field, heading, span, widest,
};
use crate::tui::tone::{Tone, toned};

/// `1 route` or `4 routes`.
pub(super) fn counted(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// What in routing reaches a provider, in the words the table's last column
/// reads in. Where routing was not read it says so, rather than reading as
/// nothing depending on the provider.
fn used_by(dependents: Option<&Dependents>) -> Vec<Span<'static>> {
    let Some(found) = dependents else {
        return vec![toned("-", Tone::Dim)];
    };
    let parts: Vec<String> = [
        (found.presentations, "presentation", "presentations"),
        (found.rewrites, "rewrite", "rewrites"),
        (found.defaults.len(), "default", "defaults"),
        (found.routes.len(), "route", "routes"),
    ]
    .into_iter()
    .filter(|(count, _, _)| *count > 0)
    .map(|(count, one, many)| counted(count, one, many))
    .collect();
    if parts.is_empty() {
        return vec![toned("nothing", Tone::Dim)];
    }
    vec![span(parts.join(" · "), Default::default())]
}

/// Where a provider's credential comes from, in words. Codex has no
/// credential of Toker's to describe: it signs with the login it shares with
/// the codex CLI.
fn sources(provider: &Provider) -> Option<String> {
    if !provider.holds_a_credential() {
        return Some("codex's own login".to_owned());
    }
    let mut found = Vec::new();
    if provider.env_set {
        found.push(match &provider.cred_env {
            Some(name) => format!("env {name}"),
            None => "env".to_owned(),
        });
    }
    if provider.keyring {
        found.push("keyring".to_owned());
    }
    if provider.literal_set {
        found.push("stored key".to_owned());
    }
    (!found.is_empty()).then(|| found.join(", "))
}

/// What a provider that supplies no credential says of the source that did
/// not (spec: UTUI#providers).
fn missing(provider: &Provider) -> String {
    match &provider.cred_env {
        Some(name) => format!("env {name} is unset"),
        None => "no source is set".to_owned(),
    }
}

/// The columns of the table, in the order they are shed: the endpoint goes
/// first, then the count of bindings.
struct Layout {
    provider: usize,
    state: usize,
    credential: usize,
    endpoint: Option<usize>,
    bindings: Option<usize>,
    gap: usize,
}

const ENDPOINT_MAX: usize = 28;

fn layout(snapshot: &Snapshot, state: &State, width: usize) -> Layout {
    let providers = &snapshot.providers;
    let provider = widest(
        providers
            .iter()
            .map(|found| found.name.as_str())
            .chain(["provider"]),
    );
    let credentials: Vec<String> = providers
        .iter()
        .map(|found| sources(found).unwrap_or_else(|| "none".to_owned()))
        .collect();
    let credential = widest(credentials.iter().map(String::as_str).chain(["credential"]));
    let endpoint = widest(
        providers
            .iter()
            .map(|found| found.upstream.as_deref().unwrap_or("built-in"))
            .chain(["endpoint"]),
    )
    .min(ENDPOINT_MAX);
    let used_by = providers
        .iter()
        .map(|found| {
            let text: String = used_by(state.dependents(&found.name).as_ref())
                .iter()
                .map(|piece| piece.content.to_string())
                .collect();
            text.chars().count()
        })
        .max()
        .unwrap_or(0)
        .max("used by".len());
    let bindings = "bindings".len();
    let fixed = provider + 5 + credential;
    for gap in [3, 2] {
        let total = |endpoint: Option<usize>, bindings: Option<usize>| {
            fixed
                + gap * 3
                + endpoint.map_or(0, |width| width + gap)
                + bindings.map_or(0, |width| width + gap)
                + used_by
        };
        for (shown_endpoint, shown_bindings) in [
            (Some(endpoint), Some(bindings)),
            (None, Some(bindings)),
            (None, None),
        ] {
            if total(shown_endpoint, shown_bindings) <= width {
                return Layout {
                    provider,
                    state: 5,
                    credential,
                    endpoint: shown_endpoint,
                    bindings: shown_bindings,
                    gap,
                };
            }
        }
    }
    Layout {
        provider,
        state: 5,
        credential,
        endpoint: None,
        bindings: None,
        gap: 2,
    }
}

fn header(layout: &Layout) -> Line<'static> {
    let mut cells = Cells::new()
        .cell(heading("provider"), layout.provider + layout.gap)
        .cell(heading("state"), layout.state + layout.gap)
        .cell(heading("credential"), layout.credential + layout.gap);
    if let Some(width) = layout.endpoint {
        cells = cells.cell(heading("endpoint"), width + layout.gap);
    }
    if let Some(width) = layout.bindings {
        cells = cells.right(heading("bindings"), width);
        cells = cells.raw(vec![Span::raw(" ".repeat(layout.gap))]);
    }
    cells.raw(heading("used by")).line()
}

fn row(provider: &Provider, state: &State, layout: &Layout, width: usize) -> Line<'static> {
    let dependents = state.dependents(&provider.name);
    let off = |text: String| {
        if provider.enabled {
            Span::raw(text)
        } else {
            toned(text, Tone::Dim)
        }
    };
    let credential = match sources(provider) {
        Some(sources) => off(sources),
        // A word beside the colour: `none` is yellow on a provider that is
        // on and so has no credential to answer with.
        None if provider.is_not_configured() => toned("none", Tone::Warn),
        None => off("none".to_owned()),
    };
    let mut cells = Cells::new()
        .cell(
            vec![off(provider.name.clone())],
            layout.provider + layout.gap,
        )
        .cell(
            vec![off(if provider.enabled { "on" } else { "off" }.to_owned())],
            layout.state + layout.gap,
        )
        .cell(vec![credential], layout.credential + layout.gap);
    if let Some(endpoint) = layout.endpoint {
        let text = provider.upstream.as_deref().unwrap_or("built-in");
        let span = if provider.upstream.is_some() {
            off(text.to_owned())
        } else {
            toned(text, Tone::Dim)
        };
        cells = cells.cell(vec![span], endpoint + layout.gap);
    }
    if let Some(width) = layout.bindings {
        let count = dependents
            .as_ref()
            .map_or_else(|| "-".to_owned(), |found| found.bindings.len().to_string());
        cells = cells
            .right(vec![off(count)], width)
            .raw(vec![Span::raw(" ".repeat(layout.gap))]);
    }
    // The last column takes what is left, clipped where it is too long.
    let used = layout.provider
        + layout.state
        + layout.credential
        + layout.gap * 3
        + layout.endpoint.map_or(0, |width| width + layout.gap)
        + layout.bindings.map_or(0, |width| width + layout.gap);
    let mut tail = used_by(dependents.as_ref());
    if !provider.enabled {
        tail = tail
            .into_iter()
            .map(|piece| toned(piece.content.to_string(), Tone::Dim))
            .collect();
    }
    cells.cell(tail, width.saturating_sub(used).max(1)).line()
}

/// The words a binding's safeguards support reads in beside it.
fn safeguards_spans(binding: &Safeguards) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw(format!("safeguards {}", binding.declared))];
    if binding.marked_ms.is_some() {
        // A refusal marked it, which reads as unsupported to the router until
        // the mark is cleared (spec: WSAFE#a-rejection-mark).
        spans.push(Span::raw(", "));
        spans.push(toned("marked unsupported", Tone::Warn));
    }
    spans
}

/// Each of a provider's bindings, with how it bills where routing said, and
/// the safeguards support of the Messages ones. The support goes on a line
/// of its own where it would not fit beside the binding, so it is never the
/// part cut off.
fn binding_lines(
    provider: &Provider,
    snapshot: &Snapshot,
    dependents: Option<&Dependents>,
    room: usize,
) -> Vec<Vec<Span<'static>>> {
    let mut ids: Vec<(String, Option<String>)> = dependents
        .map(|found| {
            found
                .bindings
                .iter()
                .map(|(id, cost)| (id.clone(), Some(cost.clone())))
                .collect()
        })
        .unwrap_or_default();
    for binding in snapshot
        .safeguards
        .iter()
        .filter(|binding| binding.provider == provider.name)
    {
        if !ids.iter().any(|(id, _)| *id == binding.binding_id) {
            ids.push((binding.binding_id.clone(), None));
        }
    }
    ids.into_iter()
        .flat_map(|(id, cost)| {
            let mut spans = vec![Span::raw(id.clone())];
            if let Some(cost) = cost.filter(|cost| !cost.is_empty()) {
                spans.push(Span::raw(" · "));
                spans.push(if cost == "billed" {
                    toned(cost, Tone::Warn)
                } else {
                    Span::raw(cost)
                });
            }
            if let Some(binding) = snapshot
                .safeguards
                .iter()
                .find(|binding| binding.binding_id == id)
            {
                let support = safeguards_spans(binding);
                let width = |spans: &[Span<'static>]| spans.iter().map(Span::width).sum::<usize>();
                if width(&spans) + 3 + width(&support) <= room {
                    spans.push(Span::raw(" · "));
                    spans.extend(support);
                } else {
                    return vec![spans, support];
                }
            }
            vec![spans]
        })
        .collect()
}

/// The selected provider's detail: label and value rows, then its controls
/// with the fix for what is wrong first.
fn detail(provider: &Provider, snapshot: &Snapshot, state: &State, width: usize) -> Detail {
    let dependents = state.dependents(&provider.name);
    let wanted = |has: bool, text: &'static str| has.then_some(text);
    let labels = [
        "state",
        "credential",
        "endpoint",
        "bindings",
        "rewrites",
        "defaults",
        "routes",
    ];
    let handshake = provider.has_handshake_settings();
    let label_width = widest(
        labels
            .into_iter()
            .chain(wanted(handshake, "refresh url"))
            .chain(wanted(handshake, "client version"))
            .chain(wanted(handshake, "version probe")),
    ) + 2;
    let mut lines: Vec<Line<'static>> = vec![Line::styled(
        provider.name.clone(),
        crate::tui::tone::bold(),
    )];

    let mut state_value = vec![Span::raw(if provider.enabled { "on" } else { "off" })];
    if provider.is_not_configured() {
        state_value.push(Span::raw(", "));
        state_value.push(toned("not configured", Tone::Warn));
    }
    lines.push(field("state", label_width, state_value));

    let credential = match sources(provider) {
        Some(sources) => vec![Span::raw(sources)],
        None => {
            let tone = if provider.enabled {
                Tone::Warn
            } else {
                Tone::Dim
            };
            let mut spans = vec![toned("none", tone)];
            if provider.enabled {
                spans.push(Span::raw(format!(" · {}", missing(provider))));
            }
            spans
        }
    };
    lines.push(field("credential", label_width, credential));

    let built_in = || vec![toned("built-in", Tone::Dim)];
    let url = |value: &Option<String>| match value {
        Some(value) => vec![Span::raw(value.clone())],
        None => built_in(),
    };
    lines.push(field("endpoint", label_width, url(&provider.upstream)));
    if handshake {
        lines.push(field(
            "refresh url",
            label_width,
            url(&provider.refresh_url),
        ));
        lines.push(field(
            "client version",
            label_width,
            match &provider.client_version {
                Some(version) => vec![Span::raw(version.clone())],
                None => vec![toned("none pinned", Tone::Dim)],
            },
        ));
        lines.push(field(
            "version probe",
            label_width,
            vec![Span::raw(if provider.probes_version() {
                "on"
            } else {
                "off"
            })],
        ));
    }

    let bindings = binding_lines(
        provider,
        snapshot,
        dependents.as_ref(),
        width.saturating_sub(label_width),
    );
    if bindings.is_empty() {
        lines.push(field("bindings", label_width, vec![toned("-", Tone::Dim)]));
    }
    for (at, spans) in bindings.into_iter().enumerate() {
        lines.push(field(
            if at == 0 { "bindings" } else { "" },
            label_width,
            spans,
        ));
    }

    match &dependents {
        None => lines.push(field(
            "routing",
            label_width,
            vec![toned("-  not read from the service", Tone::Dim)],
        )),
        Some(found) => {
            lines.push(field(
                "rewrites",
                label_width,
                dependent_row(
                    found.rewrites,
                    found
                        .rewrites_by_protocol
                        .iter()
                        .map(|(protocol, count)| format!("{protocol} {count}"))
                        .collect(),
                ),
            ));
            lines.push(field(
                "defaults",
                label_width,
                dependent_row(found.defaults.len(), found.defaults.clone()),
            ));
            lines.push(field(
                "routes",
                label_width,
                dependent_row(found.routes.len(), found.routes.clone()),
            ));
        }
    }

    let marked = snapshot
        .safeguards
        .iter()
        .any(|binding| binding.provider == provider.name && binding.marked_ms.is_some());
    let mut controls: Vec<(String, &str)> = Vec::new();
    let mut fix = None;
    if provider.holds_a_credential() {
        let what = if provider.has_credential() {
            "change key"
        } else {
            "enter key"
        };
        if provider.is_not_configured() {
            fix = Some(("e".to_owned(), what));
        } else {
            controls.push(("e".to_owned(), what));
        }
    }
    controls.insert(
        0,
        (
            "space".to_owned(),
            if provider.enabled {
                "turn off"
            } else {
                "turn on"
            },
        ),
    );
    controls.push(("u".to_owned(), "endpoint"));
    if handshake {
        controls.push(("f".to_owned(), "refresh url"));
        controls.push(("v".to_owned(), "client version"));
        controls.push((
            "p".to_owned(),
            if provider.probes_version() {
                "probe off"
            } else {
                "probe on"
            },
        ));
    }
    if marked {
        controls.push(("c".to_owned(), "clear safeguards mark"));
    }
    if dependents
        .as_ref()
        .is_some_and(|found| !found.routes.is_empty())
    {
        controls.push(("enter".to_owned(), "open route"));
    }
    let items: Vec<(&str, &str)> = fix
        .iter()
        .chain(controls.iter())
        .map(|(key, what)| (key.as_str(), *what))
        .collect();
    let first_control = lines.len();
    lines.extend(controls_in(&items, width));
    let fix_spans = fix
        .as_ref()
        .map(|(key, what)| vec![Span::raw(format!("[{key}] {what}"))])
        .unwrap_or_default();
    Detail::reading(lines, first_control, vec![fix_spans], "")
}

/// A detail row of what depends on a provider: how many, then which.
fn dependent_row(count: usize, names: Vec<String>) -> Vec<Span<'static>> {
    if count == 0 {
        return vec![toned("none", Tone::Dim)];
    }
    let mut spans = vec![Span::raw(format!("{count}  "))];
    spans.push(Span::raw(names.join(" · ")));
    spans
}

/// Draw the page: the table, the selected provider's detail under it, and a
/// form where one is open. Returns where each row landed.
pub(super) fn draw(
    frame: &mut Frame,
    body: Rect,
    state: &State,
    snapshot: &Snapshot,
) -> Vec<RowHit> {
    let width = usize::from(body.width.saturating_sub(4));
    let layout = layout(snapshot, state, width);
    let selected = state.selected();
    let mut lines: Vec<BodyLine> = snapshot
        .providers
        .iter()
        .enumerate()
        .map(|(index, provider)| BodyLine::of(row(provider, state, &layout, width), index))
        .collect();
    let mut detail = snapshot
        .providers
        .get(selected)
        .map(|provider| detail(provider, snapshot, state, width))
        .unwrap_or_else(|| Detail::whole(Vec::new()));
    if let Some(editing) = state.editing()
        && matches!(state.row(), Some(Row::Provider(_)))
        && !matches!(editing.target, Target::Setting(_))
    {
        let at = (selected + 1).min(lines.len());
        lines.splice(
            at..at,
            form::lines(editing, snapshot, vec![Span::raw("    ")], width),
        );
    }
    if lines.is_empty() {
        detail = Detail::whole(Vec::new());
    }
    let table = Table {
        header: header(&layout),
        body: lines,
    };
    let need = 1 + table.body.len().max(1);
    let split = if detail.has_reading() {
        table::split_reading(body, need, detail.len())
    } else {
        table::split(body, need, detail.len())
    };
    let hits = table::draw_table(
        frame,
        split.table,
        "PROVIDERS",
        split.chrome,
        table,
        Some(selected),
    );
    if split.detail.height > 0 {
        let lines = detail.shed(
            split.detail_rows,
            usize::from(split.detail.width.saturating_sub(4)),
        );
        if split.bare {
            table::draw_bare(frame, split.detail, lines);
        } else {
            table::draw_detail(frame, split.detail, "PROVIDER", split.chrome, lines);
        }
    }
    hits.rows
}
