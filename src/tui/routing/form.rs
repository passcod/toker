//! Typed forms for the routing page.
//!
//! A form edits one record's JSON in place, field by field, from a schema of
//! the shapes the daemon's document types accept: text, numbers, switches,
//! enum choices, tagged unions (matcher kind, presentation source, candidate
//! model), optional groups, ordered lists, maps and a route's candidate
//! chain. What it saves is the same typed record the daemon's edit operations
//! take, so the daemon stays the validator of what is acceptable. Keys the
//! schema does not name are left as they were.

use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Map, Value, json};

/// What a field holds and how it is edited.
#[derive(Debug, Clone, Copy)]
pub(super) enum Node {
    Text,
    /// Text where an empty entry means the field is absent.
    OptText,
    Int,
    OptInt,
    /// A switch, with the value a new one starts at.
    Bool(bool),
    Choice(&'static [&'static str]),
    /// A binding id, completed from the bindings toker implements.
    Binding,
    /// A backend model id, completed from the offers of the nearest binding.
    Model,
    /// The id of another record in the draft, completed from those ids.
    Ref {
        kind: &'static str,
        optional: bool,
    },
    Object(&'static [Field]),
    OptObject(&'static [Field]),
    /// A tagged union: the tag names a variant, each with its own fields.
    Union {
        tag: &'static str,
        variants: &'static [Variant],
        optional: bool,
    },
    List(&'static Node),
    /// A set of strings chosen from a fixed list, kept in list order.
    Flags(&'static [&'static str]),
    /// An object whose keys are the user's and whose values share a shape.
    Map(&'static Node),
    /// A route's candidates: the first, then each step's edge and candidate.
    Chain,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Field {
    key: &'static str,
    label: &'static str,
    node: Node,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Variant {
    name: &'static str,
    fields: &'static [Field],
}

const fn field(key: &'static str, label: &'static str, node: Node) -> Field {
    Field { key, label, node }
}

const fn variant(name: &'static str, fields: &'static [Field]) -> Variant {
    Variant { name, fields }
}

const PROTOCOL: Node = Node::Choice(&["anthropic_messages", "openai_chat", "openai_responses"]);
const LANE_TRANSITION: Node =
    Node::Choice(&["preserve_pinned", "move_when_cold", "allow_immediate"]);
const CACHE_TRANSITION: Node =
    Node::Choice(&["preserve_warm", "allow_when_cold", "allow_cache_loss"]);
const CONSENT: Node = Node::Choice(&["deny", "allow"]);
const FAILURES: &[&str] = &[
    "authentication",
    "model_not_found",
    "rate_limit",
    "provider_quota",
    "overload",
    "server_error",
    "transport_before_accept",
    "ambiguous_transport",
];

const SCOPE: Node = Node::Union {
    tag: "kind",
    optional: false,
    variants: &[
        variant("protocol", &[field("protocol", "protocol", PROTOCOL)]),
        variant(
            "profile",
            &[
                field("profile", "profile", Node::Text),
                field("protocol", "protocol", PROTOCOL),
            ],
        ),
    ],
};

const MATCHER: Node = Node::Union {
    tag: "kind",
    optional: false,
    variants: &[
        variant("exact", &[field("value", "value", Node::Text)]),
        variant("glob", &[field("value", "pattern", Node::Text)]),
        variant("regex", &[field("value", "pattern", Node::Text)]),
        variant("revision_one_model", &[field("value", "model", Node::Text)]),
        variant(
            "revision_one_family",
            &[field("value", "family", Node::Text)],
        ),
    ],
};

const ARGUMENT: Node = Node::Union {
    tag: "kind",
    optional: false,
    variants: &[
        variant("literal", &[field("value", "value", Node::Text)]),
        variant("requested_model", &[]),
        variant("regex_capture_name", &[field("name", "name", Node::Text)]),
        variant("regex_capture_index", &[field("index", "index", Node::Int)]),
    ],
};

const ROUTE_REFERENCE: Node = Node::Object(&[
    field(
        "route_id",
        "route",
        Node::Ref {
            kind: "routes",
            optional: false,
        },
    ),
    field("parameters", "arguments", Node::Map(&ARGUMENT)),
]);

const ORDERING: Node = Node::Union {
    tag: "kind",
    optional: false,
    variants: &[
        variant("catalogue_chronology", &[]),
        variant(
            "classified_version",
            &[field(
                "classifier_id",
                "classifier",
                Node::Ref {
                    kind: "classifiers",
                    optional: false,
                },
            )],
        ),
        variant(
            "explicit",
            &[field("model_ids", "models", Node::List(&Node::Model))],
        ),
    ],
};

const OFFER_SELECTOR: Node = Node::Object(&[
    field("binding_id", "binding", Node::Binding),
    field("include", "include", Node::List(&MATCHER)),
    field("exclude", "exclude", Node::List(&MATCHER)),
    field("ordering", "ordering", ORDERING),
    field("keep", "keep", Node::Int),
]);

const CANDIDATE_SELECTOR: Node = Node::Object(&[
    field("include", "include", Node::List(&MATCHER)),
    field("exclude", "exclude", Node::List(&MATCHER)),
    field("ordering", "ordering", ORDERING),
]);

const CODEX_MODEL: &[Field] = &[
    field("description", "description", Node::OptText),
    field(
        "supported_reasoning_levels",
        "reasoning levels",
        Node::List(&Node::Object(&[
            field("effort", "effort", Node::Text),
            field("description", "description", Node::Text),
        ])),
    ),
    field("default_reasoning_level", "default level", Node::OptText),
    field(
        "visibility",
        "visibility",
        Node::Choice(&["list", "hide", "none"]),
    ),
    field(
        "shell_type",
        "shell type",
        Node::Choice(&["unified_exec", "disabled"]),
    ),
    field("supported_in_api", "supported in API", Node::Bool(true)),
    field("support_verbosity", "supports verbosity", Node::Bool(false)),
    field("priority", "priority", Node::Int),
    field("context_window", "context window", Node::OptInt),
    field("max_context_window", "max context window", Node::OptInt),
];

const METADATA: Node = Node::Union {
    tag: "adapter",
    optional: true,
    variants: &[
        variant(
            "claude",
            &[field("behaves_as", "behaves as", Node::OptText)],
        ),
        variant("openai", &[]),
        variant(
            "codex",
            &[field("model", "codex model", Node::OptObject(CODEX_MODEL))],
        ),
    ],
};

const PRESENTATION_SOURCE: Node = Node::Union {
    tag: "kind",
    optional: false,
    variants: &[
        variant(
            "exact_offer",
            &[
                field("binding_id", "binding", Node::Binding),
                field("model_id", "model", Node::Model),
            ],
        ),
        variant(
            "offer_selector",
            &[
                field("selector", "selector", OFFER_SELECTOR),
                field("label_template", "label template", Node::OptText),
            ],
        ),
        variant("virtual", &[field("target", "route", ROUTE_REFERENCE)]),
    ],
};

const CANDIDATE_MODEL: Node = Node::Union {
    tag: "kind",
    optional: false,
    variants: &[
        variant("exact", &[field("model_id", "model", Node::Model)]),
        variant("requested_model", &[]),
        variant("template", &[field("template", "template", Node::Text)]),
        variant(
            "offer_selector",
            &[field("selector", "selector", CANDIDATE_SELECTOR)],
        ),
    ],
};

const CANDIDATE: &[Field] = &[
    field("id", "id", Node::Text),
    field("binding_id", "binding", Node::Binding),
    field("model", "model", CANDIDATE_MODEL),
    field(
        "upgrade_policy_id",
        "upgrade policy",
        Node::Ref {
            kind: "upgrades",
            optional: true,
        },
    ),
    field(
        "alias_policy",
        "alias rules",
        Node::OptObject(&[field(
            "rules",
            "rules",
            Node::List(&Node::Object(&[
                field("matcher", "matches", MATCHER),
                field("exclude", "except", Node::List(&MATCHER)),
                field("replacement", "replacement", Node::Text),
            ])),
        )]),
    ),
    field(
        "required_capabilities",
        "required capabilities",
        Node::Object(&[
            field("sampling", "sampling", Node::Bool(false)),
            field(
                "system_in_messages",
                "system in messages",
                Node::Bool(false),
            ),
            field("thinking_replay", "thinking replay", Node::Bool(false)),
            field("images", "images", Node::Bool(false)),
            field("tools", "tools", Node::Bool(false)),
            field("tool_choice_none", "tool choice none", Node::Bool(false)),
            field("tool_choice_named", "tool choice named", Node::Bool(false)),
            field(
                "tool_choice_unnamed",
                "tool choice unnamed",
                Node::Bool(false),
            ),
            field("tool_choice_other", "tool choice other", Node::Bool(false)),
        ]),
    ),
    field("enabled", "enabled", Node::Bool(true)),
    field(
        "health_circuits",
        "health circuits",
        Node::List(&Node::Object(&[
            field("failure", "failure", Node::Choice(FAILURES)),
            field("duration_seconds", "seconds open", Node::Int),
        ])),
    ),
];

const EDGE: &[Field] = &[
    field(
        "preflight",
        "fallback before send",
        Node::Choice(&["disabled", "enabled"]),
    ),
    field("post_send", "fallback after send on", Node::Flags(FAILURES)),
    field("billed_boundary", "may start billed spend", CONSENT),
    field(
        "ambiguous_transport",
        "may retry an ambiguous send",
        CONSENT,
    ),
    field("cache_transition", "cache transition", CACHE_TRANSITION),
];

const PRESENTATION: &[Field] = &[
    field("id", "id", Node::Text),
    field("scope", "scope", SCOPE),
    field("slug", "slug", Node::Text),
    field("label", "label", Node::OptText),
    field("description", "description", Node::OptText),
    field("enabled", "enabled", Node::Bool(true)),
    field("shadow_native", "shadows a native model", Node::Bool(false)),
    field("source", "source", PRESENTATION_SOURCE),
    field("metadata", "adapter metadata", METADATA),
];

const ROUTE: &[Field] = &[
    field("id", "id", Node::Text),
    field("display_name", "name", Node::Text),
    field("description", "description", Node::OptText),
    field("parameters", "parameters", Node::List(&Node::Text)),
    field("chain", "candidates", Node::Chain),
    field("lane_transition", "lane transition", LANE_TRANSITION),
    // Saved through its own edit operation rather than inside the route, so
    // setting it can raise the document's format with it.
    field(
        "honours_server_safeguards",
        "honours server safeguards",
        Node::Bool(false),
    ),
];

const DEFAULT: &[Field] = &[
    field("scope", "scope", SCOPE),
    field("target", "route", ROUTE_REFERENCE),
];

const REWRITE: &[Field] = &[
    field("id", "id", Node::Text),
    field("scope", "scope", SCOPE),
    field("priority", "priority", Node::Int),
    field("model_matcher", "matches", MATCHER),
    field("target", "route", ROUTE_REFERENCE),
];

const CLASSIFIER: &[Field] = &[
    field("id", "id", Node::Text),
    field("binding_id", "binding", Node::Binding),
    field("model_matcher", "matches", MATCHER),
    field("family_template", "family template", Node::Text),
    field("version_template", "version template", Node::Text),
    field(
        "version_parser",
        "version parser",
        Node::Union {
            tag: "kind",
            optional: false,
            variants: &[
                variant("semver", &[]),
                variant(
                    "date",
                    &[field(
                        "format",
                        "format",
                        Node::Choice(&["yyyy_mm_dd", "yyyymmdd"]),
                    )],
                ),
                variant(
                    "numeric_tuple",
                    &[
                        field("components", "components", Node::Int),
                        field("separator", "separator", Node::Text),
                    ],
                ),
                variant(
                    "legacy_numeric_tuple",
                    &[field("separator", "separator", Node::Text)],
                ),
            ],
        },
    ),
];

const UPGRADE: &[Field] = &[
    field("id", "id", Node::Text),
    field(
        "classifier_id",
        "classifier",
        Node::Ref {
            kind: "classifiers",
            optional: false,
        },
    ),
    field("enabled", "enabled", Node::Bool(true)),
    field(
        "minimum_learning",
        "minimum learning",
        Node::Union {
            tag: "kind",
            optional: false,
            variants: &[
                variant("none", &[]),
                variant("distinct_days", &[field("days", "days", Node::Int)]),
                variant("explicit_promotion", &[]),
                variant("revision_one_adaptive", &[]),
            ],
        },
    ),
    field(
        "allow_manual_promotion",
        "allow manual promotion",
        Node::Bool(false),
    ),
    field(
        "inventory_sources",
        "inventory sources",
        Node::Flags(&["catalogue", "served", "learned"]),
    ),
    field("lane_transition", "lane transition", LANE_TRANSITION),
];

const COMPACTION: &[Field] = &[
    field("target", "route", ROUTE_REFERENCE),
    field("lane_transition", "lane transition", LANE_TRANSITION),
    field("cache_transition", "cache transition", CACHE_TRANSITION),
];

const DESCRIPTION: &[Field] = &[field("description", "description", Node::OptText)];

const PROMOTE: &[Field] = &[
    field("binding_id", "binding", Node::Binding),
    field("model_id", "model", Node::Model),
];

const PREVIEW: &[Field] = &[
    field("protocol", "protocol", PROTOCOL),
    field("profile", "profile", Node::OptText),
    field("model", "model", Node::OptText),
    field("lane_key", "lane key", Node::OptText),
    field("prompt_bound", "prompt bound", Node::Int),
    field("message_count", "message count", Node::OptInt),
    field("tool_count", "tool count", Node::Int),
    field(
        "asks_server_safeguards",
        "asks for server safeguards",
        Node::Bool(false),
    ),
];

/// What a form is for, which decides its schema and what saving it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Purpose {
    Record { kind: &'static str, adding: bool },
    Description,
    Promote,
    Preview,
}

impl Purpose {
    pub(super) fn title(self) -> &'static str {
        match self {
            Purpose::Record { kind, adding } => match (kind, adding) {
                ("presentations", false) => "EDIT PRESENTATION",
                ("presentations", true) => "ADD PRESENTATION",
                ("routes", false) => "EDIT ROUTE",
                ("routes", true) => "ADD ROUTE",
                ("defaults", false) => "EDIT DEFAULT",
                ("defaults", true) => "ADD DEFAULT",
                ("rewrites", false) => "EDIT REWRITE",
                ("rewrites", true) => "ADD REWRITE",
                ("classifiers", false) => "EDIT CLASSIFIER",
                ("classifiers", true) => "ADD CLASSIFIER",
                ("upgrades", false) => "EDIT UPGRADE",
                ("upgrades", true) => "ADD UPGRADE",
                ("compaction", false) => "EDIT COMPACTION",
                ("compaction", true) => "ADD COMPACTION",
                (_, false) => "EDIT POLICY RECORD",
                (_, true) => "ADD POLICY RECORD",
            },
            Purpose::Description => "EDIT DRAFT DESCRIPTION",
            Purpose::Promote => "PROMOTE LEARNED MODEL",
            Purpose::Preview => "ROUTING PREVIEW",
        }
    }

    fn schema(self) -> &'static [Field] {
        match self {
            Purpose::Record { kind, .. } => match kind {
                "presentations" => PRESENTATION,
                "routes" => ROUTE,
                "defaults" => DEFAULT,
                "rewrites" => REWRITE,
                "classifiers" => CLASSIFIER,
                "upgrades" => UPGRADE,
                "compaction" => COMPACTION,
                _ => &[],
            },
            Purpose::Description => DESCRIPTION,
            Purpose::Promote => PROMOTE,
            Purpose::Preview => PREVIEW,
        }
    }

    /// Short forms answer one question and Enter submits them. A record form
    /// has many fields, so Enter there moves on.
    fn enter_submits(self) -> bool {
        !matches!(self, Purpose::Record { .. })
    }

    fn help(self) -> &'static str {
        match self {
            Purpose::Record { .. } => {
                "↑/↓ field · ←/→ change · tab complete · ctrl+s save · esc cancel"
            }
            Purpose::Description => "enter save · esc cancel",
            Purpose::Promote => "tab complete from a daemon offer · enter record · esc cancel",
            Purpose::Preview => {
                "content-free request facts only · space toggle · enter preview · esc cancel"
            }
        }
    }
}

/// What the daemon last said that completion draws on.
#[derive(Debug, Clone, Default)]
pub(super) struct Context {
    /// Every binding toker implements.
    pub(super) bindings: Vec<String>,
    /// (binding, model) pairs the catalogues offer.
    pub(super) offers: Vec<(String, String)>,
    /// Record ids in the draft, by collection.
    pub(super) ids: BTreeMap<&'static str, Vec<String>>,
    /// Draft diagnostics against the record being edited, as (path inside
    /// the record, code).
    pub(super) diagnostics: Vec<(String, String)>,
}

/// One step into a value.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    Key(String),
    Index(usize),
}

fn key(name: &str) -> Seg {
    Seg::Key(name.to_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Input {
    Text,
    OptText,
    Int,
    OptInt,
    Binding,
    Model,
    Ref {
        kind: &'static str,
        optional: bool,
    },
    /// A map entry's key: renaming moves the entry.
    MapKey,
}

impl Input {
    fn numeric(self) -> bool {
        matches!(self, Input::Int | Input::OptInt)
    }
    fn nullable(self) -> bool {
        matches!(
            self,
            Input::OptText | Input::OptInt | Input::Ref { optional: true, .. }
        )
    }
}

#[derive(Debug, Clone)]
enum Op {
    /// A label with nothing to edit.
    Heading,
    Input(Input),
    Toggle,
    Choice(&'static [&'static str]),
    /// A tagged union's variant, switching rebuilds the variant's fields.
    Tag {
        node: Node,
    },
    /// Whether an optional group or union is present.
    Presence {
        node: Node,
    },
    Flag(&'static str),
    /// Appends to the list or map at the row's path.
    Add {
        node: Node,
    },
    /// The header of a compound list item; only the item keys act on it.
    ItemHeader,
}

#[derive(Debug, Clone)]
enum ItemKind {
    List,
    Map(String),
    Chain,
}

#[derive(Debug, Clone)]
struct Item {
    /// The list, map or chain's own path.
    owner: Vec<Seg>,
    index: usize,
    len: usize,
    kind: ItemKind,
}

#[derive(Debug, Clone)]
struct Row {
    path: Vec<Seg>,
    depth: usize,
    label: String,
    op: Op,
    item: Option<Item>,
}

impl Row {
    fn focusable(&self) -> bool {
        !matches!(self.op, Op::Heading)
    }

    fn text_like(&self) -> bool {
        matches!(self.op, Op::Input(_))
    }
}

/// What a key did to the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    Keep,
    Cancel,
    Submit,
}

/// One rendered line of a form.
#[derive(Debug, Clone)]
pub(super) struct FormLine {
    pub(super) text: String,
    pub(super) focused: bool,
    /// The field a click on this line focuses; a line that only informs
    /// (a heading, the completions) has none.
    pub(super) row: Option<usize>,
}

#[derive(Debug, Clone)]
pub(super) struct Form {
    purpose: Purpose,
    value: Value,
    focus: usize,
    /// The focused input's text and cursor, written through as it changes.
    buffer: String,
    cursor: usize,
    /// While Tab cycles completions: what was typed, and the one showing.
    completing: Option<(String, usize)>,
    context: Context,
    note: Option<String>,
}

impl Form {
    pub(super) fn new(purpose: Purpose, value: Value, context: Context) -> Form {
        let mut form = Form {
            purpose,
            value,
            focus: 0,
            buffer: String::new(),
            cursor: 0,
            completing: None,
            context,
            note: None,
        };
        form.focus_first();
        form
    }

    pub(super) fn purpose(&self) -> Purpose {
        self.purpose
    }

    pub(super) fn value(&self) -> &Value {
        &self.value
    }

    pub(super) fn title(&self) -> &'static str {
        self.purpose.title()
    }

    pub(super) fn help(&self) -> &'static str {
        self.purpose.help()
    }

    /// A refusal or hint to show under the form.
    pub(super) fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for field in self.purpose.schema() {
            walk(
                &field.node,
                field.label,
                vec![key(field.key)],
                0,
                &self.value,
                &mut rows,
            );
        }
        rows
    }

    fn focus_first(&mut self) {
        let rows = self.rows();
        self.focus = rows.iter().position(Row::focusable).unwrap_or(0);
        self.load_buffer(&rows);
    }

    /// Put the focused input's current value in the edit buffer.
    fn load_buffer(&mut self, rows: &[Row]) {
        self.completing = None;
        self.buffer = match rows.get(self.focus) {
            Some(row) => match &row.op {
                Op::Input(Input::MapKey) => match &row.item {
                    Some(Item {
                        kind: ItemKind::Map(name),
                        ..
                    }) => name.clone(),
                    _ => String::new(),
                },
                Op::Input(_) => match get(&self.value, &row.path) {
                    Value::String(text) => text.clone(),
                    Value::Number(number) => number.to_string(),
                    _ => String::new(),
                },
                _ => String::new(),
            },
            None => String::new(),
        };
        self.cursor = self.buffer.chars().count();
    }

    #[cfg(test)]
    fn set_focus(&mut self, focus: usize) {
        let rows = self.rows();
        self.focus = focus.min(rows.len().saturating_sub(1));
        self.load_buffer(&rows);
    }

    /// Focus a field by its row (a click on its line).
    pub(super) fn focus_row(&mut self, row: usize) {
        let rows = self.rows();
        if rows.get(row).is_some_and(Row::focusable) {
            self.focus = row;
            self.load_buffer(&rows);
        }
    }

    /// Move the focus up or down by whole fields, as the wheel does.
    pub(super) fn step(&mut self, delta: isize) {
        for _ in 0..delta.unsigned_abs() {
            self.step_focus(delta.signum());
        }
    }

    fn step_focus(&mut self, delta: isize) {
        let rows = self.rows();
        let mut at = self.focus as isize;
        loop {
            at += delta;
            if at < 0 || at as usize >= rows.len() {
                return;
            }
            if rows[at as usize].focusable() {
                self.focus = at as usize;
                self.load_buffer(&rows);
                return;
            }
        }
    }

    /// The form's lines, one per row, with completions under the focused
    /// input.
    pub(super) fn lines(&self) -> Vec<FormLine> {
        let rows = self.rows();
        let mut lines = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let focused = index == self.focus;
            lines.push(FormLine {
                text: self.row_text(row, focused),
                focused,
                row: row.focusable().then_some(index),
            });
            if focused && let Some(text) = self.suggestion_line() {
                lines.push(FormLine {
                    text: format!("    {text}"),
                    focused: false,
                    row: None,
                });
            }
        }
        lines
    }

    #[cfg(test)]
    fn focus(&self) -> usize {
        self.focus
    }

    /// The completions for the focused input, on one line.
    fn suggestion_line(&self) -> Option<String> {
        let rows = self.rows();
        let row = rows.get(self.focus)?;
        let Op::Input(input) = &row.op else {
            return None;
        };
        let (base, selected) = self
            .completing
            .clone()
            .map_or((self.buffer.clone(), None), |(base, at)| (base, Some(at)));
        let matches = self.matches(row, *input, &base);
        if matches.is_empty() {
            return None;
        }
        let shown: Vec<String> = matches
            .iter()
            .take(6)
            .enumerate()
            .map(|(index, name)| {
                if Some(index) == selected {
                    format!("[{name}]")
                } else {
                    name.clone()
                }
            })
            .collect();
        let more = matches.len().saturating_sub(shown.len());
        Some(format!(
            "Tab: {}{}",
            shown.join(" | "),
            if more > 0 {
                format!(" (+{more})")
            } else {
                String::new()
            }
        ))
    }

    fn row_text(&self, row: &Row, focused: bool) -> String {
        let indent = "  ".repeat(row.depth);
        let mark = if focused { "> " } else { "  " };
        let diagnostic = self
            .diagnostic_for(row)
            .map_or(String::new(), |code| format!("  ! {code}"));
        let body = match &row.op {
            Op::Heading => row.label.to_uppercase(),
            Op::ItemHeader => {
                let id = ["id", "name"]
                    .into_iter()
                    .find_map(|name| {
                        get(&self.value, &[row.path.clone(), vec![key(name)]].concat()).as_str()
                    })
                    .filter(|text| !text.is_empty());
                match id {
                    Some(id) => format!("{} {id}", row.label),
                    None => row.label.clone(),
                }
            }
            Op::Input(input) => {
                let text = if focused {
                    let mut chars: Vec<char> = self.buffer.chars().collect();
                    chars.insert(self.cursor.min(chars.len()), '\u{258f}');
                    chars.into_iter().collect()
                } else {
                    match get(&self.value, &row.path) {
                        Value::String(text) => text.clone(),
                        Value::Number(number) => number.to_string(),
                        _ if matches!(input, Input::MapKey) => match &row.item {
                            Some(Item {
                                kind: ItemKind::Map(name),
                                ..
                            }) => name.clone(),
                            _ => String::new(),
                        },
                        _ => String::new(),
                    }
                };
                // A value still being typed is not judged until it is left.
                let unoffered = !focused && self.unoffered(row, *input);
                format!(
                    "{}: {}{}",
                    row.label,
                    if text.is_empty() && !focused && input.nullable() {
                        "(none)".to_owned()
                    } else {
                        text
                    },
                    if unoffered { "  (not offered)" } else { "" }
                )
            }
            Op::Toggle => format!(
                "[{}] {}",
                if get(&self.value, &row.path) == &Value::Bool(true) {
                    "x"
                } else {
                    " "
                },
                row.label
            ),
            Op::Choice(_) => format!(
                "{}: < {} >",
                row.label,
                get(&self.value, &row.path).as_str().unwrap_or("?")
            ),
            Op::Tag { node } => {
                let tag = tag_name(node).unwrap_or("kind");
                format!(
                    "{}: < {} >",
                    row.label,
                    get(&self.value, &[row.path.clone(), vec![key(tag)]].concat())
                        .as_str()
                        .unwrap_or("?")
                )
            }
            Op::Presence { .. } => format!(
                "[{}] {}",
                if get(&self.value, &row.path).is_null() {
                    " "
                } else {
                    "x"
                },
                row.label
            ),
            Op::Flag(option) => format!(
                "[{}] {}",
                if get(&self.value, &row.path)
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(option)))
                {
                    "x"
                } else {
                    " "
                },
                option.replace('_', " ")
            ),
            Op::Add { .. } => format!("+ add {}", row.label),
        };
        let fixed = if self.locked(row) && !matches!(row.op, Op::Heading) {
            "  (fixed)"
        } else {
            ""
        };
        format!("{mark}{indent}{body}{fixed}{diagnostic}")
    }

    fn diagnostic_for(&self, row: &Row) -> Option<&str> {
        let path = path_text(&row.path);
        self.context
            .diagnostics
            .iter()
            .find(|(at, _)| *at == path)
            .map(|(_, code)| code.as_str())
    }

    /// Whether a completed field holds a value the catalogues do not offer.
    /// With no candidates known the field cannot be judged, so it is not
    /// marked.
    fn unoffered(&self, row: &Row, input: Input) -> bool {
        let value = match get(&self.value, &row.path).as_str() {
            Some(text) if !text.is_empty() => text,
            _ => return false,
        };
        let all = self.candidates(row, input);
        !all.is_empty() && !all.iter().any(|candidate| candidate == value)
    }

    fn candidates(&self, row: &Row, input: Input) -> Vec<String> {
        match input {
            Input::Binding => self.context.bindings.clone(),
            Input::Model => {
                let binding = self.nearest_binding(&row.path);
                let mut models: Vec<String> = self
                    .context
                    .offers
                    .iter()
                    .filter(|(offered, _)| binding.as_deref().is_none_or(|b| b == offered))
                    .map(|(_, model)| model.clone())
                    .collect();
                models.sort();
                models.dedup();
                models
            }
            Input::Ref { kind, .. } => self.context.ids.get(kind).cloned().unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn matches(&self, row: &Row, input: Input, typed: &str) -> Vec<String> {
        let needle = typed.to_lowercase();
        self.candidates(row, input)
            .into_iter()
            .filter(|candidate| candidate.to_lowercase().contains(&needle))
            .collect()
    }

    /// The binding the field's record names: the nearest `binding_id` on the
    /// way up from the field, so a model completes against its own binding.
    fn nearest_binding(&self, path: &[Seg]) -> Option<String> {
        for end in (0..path.len()).rev() {
            let mut probe = path[..end].to_vec();
            probe.push(key("binding_id"));
            if let Some(binding) = get(&self.value, &probe).as_str()
                && !binding.is_empty()
            {
                return Some(binding.to_owned());
            }
        }
        None
    }

    /// Whether saving would make sense to try: not when a required id or
    /// name is blank, which the daemon would only refuse.
    pub(super) fn problem(&self) -> Option<String> {
        if let Purpose::Record { .. } = self.purpose {
            for name in ["id", "slug"] {
                if let Some(text) = self.value.get(name).and_then(Value::as_str)
                    && text.is_empty()
                {
                    return Some(format!("{name} is empty"));
                }
            }
        }
        None
    }

    pub(super) fn key(&mut self, event: KeyEvent) -> Outcome {
        self.note = None;
        let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
        let alt = event.modifiers.contains(KeyModifiers::ALT);
        let rows = self.rows();
        let Some(row) = rows.get(self.focus).cloned() else {
            return if event.code == KeyCode::Esc {
                Outcome::Cancel
            } else {
                Outcome::Keep
            };
        };
        if self.locked(&row) && edits(&event, &row) {
            self.note = Some("fixed once added: add a new record to change it".to_owned());
            return Outcome::Keep;
        }
        match event.code {
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Enter if ctrl => return self.submit(),
            KeyCode::Char('s') if ctrl => return self.submit(),
            KeyCode::Up if ctrl || alt => self.move_item(&row, -1),
            KeyCode::Down if ctrl || alt => self.move_item(&row, 1),
            KeyCode::Char('d') if ctrl => self.remove_item(&row),
            KeyCode::Up | KeyCode::BackTab => self.step_focus(-1),
            KeyCode::Down => self.step_focus(1),
            KeyCode::Tab => self.tab(&row),
            KeyCode::Enter => {
                if self.purpose.enter_submits() && row.text_like() {
                    return self.submit();
                }
                self.activate(&row, true);
            }
            KeyCode::Left => self.arrow(&row, -1),
            KeyCode::Right => self.arrow(&row, 1),
            KeyCode::Home if row.text_like() => self.cursor = 0,
            KeyCode::End if row.text_like() => self.cursor = self.buffer.chars().count(),
            KeyCode::Backspace if row.text_like() => self.erase(&row, true),
            KeyCode::Delete if row.text_like() => self.erase(&row, false),
            KeyCode::Char(' ') if !row.text_like() => self.activate(&row, false),
            KeyCode::Char(character) if row.text_like() && !ctrl && !alt => {
                self.insert(&row, character);
            }
            _ => {}
        }
        Outcome::Keep
    }

    /// What identifies an existing record cannot change in an edit: the edit
    /// names the record by it, so a different value would name another one.
    fn locked(&self, row: &Row) -> bool {
        let Purpose::Record {
            kind,
            adding: false,
        } = self.purpose
        else {
            return false;
        };
        match row.path.first() {
            Some(Seg::Key(name)) => name == "id" || (kind == "defaults" && name == "scope"),
            _ => false,
        }
    }

    fn submit(&mut self) -> Outcome {
        if let Some(problem) = self.problem() {
            self.note = Some(problem);
            return Outcome::Keep;
        }
        Outcome::Submit
    }

    /// Tab completes a completable input, and otherwise moves on.
    fn tab(&mut self, row: &Row) {
        let Op::Input(input) = row.op else {
            self.step_focus(1);
            return;
        };
        let typed = self
            .completing
            .as_ref()
            .map_or(self.buffer.clone(), |(base, _)| base.clone());
        let matches = self.matches(row, input, &typed);
        if matches.is_empty() {
            self.step_focus(1);
            return;
        }
        let next = self
            .completing
            .as_ref()
            .map_or(0, |(_, at)| (at + 1) % matches.len());
        self.buffer = matches[next].clone();
        self.cursor = self.buffer.chars().count();
        self.completing = Some((typed, next));
        self.write_buffer(row);
    }

    fn insert(&mut self, row: &Row, character: char) {
        if let Op::Input(input) = row.op
            && input.numeric()
            && !(character.is_ascii_digit() || (character == '-' && self.cursor == 0))
        {
            return;
        }
        self.completing = None;
        let at = byte_index(&self.buffer, self.cursor);
        self.buffer.insert(at, character);
        self.cursor += 1;
        self.write_buffer(row);
    }

    fn erase(&mut self, row: &Row, backwards: bool) {
        self.completing = None;
        if backwards {
            if self.cursor == 0 {
                return;
            }
            self.cursor -= 1;
        } else if self.cursor >= self.buffer.chars().count() {
            return;
        }
        let at = byte_index(&self.buffer, self.cursor);
        self.buffer.remove(at);
        self.write_buffer(row);
    }

    /// Write the buffer through to the value. A number that does not parse
    /// yet (a lone minus) leaves the value as it was.
    fn write_buffer(&mut self, row: &Row) {
        let Op::Input(input) = row.op else { return };
        if input == Input::MapKey {
            self.rename_entry(row);
            return;
        }
        let value = if input.numeric() {
            match self.buffer.parse::<i64>() {
                Ok(number) => Value::from(number),
                Err(_) if self.buffer.is_empty() && input == Input::OptInt => Value::Null,
                Err(_) if self.buffer.is_empty() => Value::from(0),
                Err(_) => return,
            }
        } else if self.buffer.is_empty() && input.nullable() {
            Value::Null
        } else {
            Value::String(self.buffer.clone())
        };
        *get_mut(&mut self.value, &row.path) = value;
    }

    fn rename_entry(&mut self, row: &Row) {
        let Some(Item {
            owner,
            kind: ItemKind::Map(old),
            ..
        }) = &row.item
        else {
            return;
        };
        let new = self.buffer.clone();
        let Value::Object(map) = get_mut(&mut self.value, owner) else {
            return;
        };
        if new.is_empty() || (new != *old && map.contains_key(&new)) {
            return;
        }
        if let Some(entry) = map.remove(old) {
            map.insert(new.clone(), entry);
        }
        // Entries stay in key order, so the renamed one may have moved.
        let rows = self.rows();
        if let Some(at) = rows.iter().position(|candidate| {
            matches!(
                &candidate.item,
                Some(Item { kind: ItemKind::Map(name), owner: other, .. })
                    if *name == new && other == owner_path(row)
            ) && matches!(candidate.op, Op::Input(Input::MapKey))
        }) {
            self.focus = at;
        }
    }

    fn arrow(&mut self, row: &Row, delta: isize) {
        match &row.op {
            Op::Input(_) => {
                let length = self.buffer.chars().count() as isize;
                self.cursor = (self.cursor as isize + delta).clamp(0, length) as usize;
            }
            Op::Choice(options) => self.cycle_choice(row, options, delta),
            Op::Tag { node } => self.cycle_tag(row, *node, delta),
            Op::Toggle | Op::Presence { .. } | Op::Flag(_) if (delta > 0) != self.is_on(row) => {
                self.activate(row, false);
            }
            _ => {}
        }
    }

    fn is_on(&self, row: &Row) -> bool {
        match &row.op {
            Op::Toggle => get(&self.value, &row.path) == &Value::Bool(true),
            Op::Presence { .. } => !get(&self.value, &row.path).is_null(),
            Op::Flag(option) => get(&self.value, &row.path)
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(option))),
            _ => false,
        }
    }

    fn cycle_choice(&mut self, row: &Row, options: &'static [&'static str], delta: isize) {
        let current = get(&self.value, &row.path)
            .as_str()
            .and_then(|text| options.iter().position(|option| *option == text));
        let next = cycle(current, options.len(), delta);
        *get_mut(&mut self.value, &row.path) = Value::from(options[next]);
    }

    fn cycle_tag(&mut self, row: &Row, node: Node, delta: isize) {
        let Node::Union { tag, variants, .. } = node else {
            return;
        };
        let current_value = get(&self.value, &row.path).clone();
        let current = current_value
            .get(tag)
            .and_then(Value::as_str)
            .and_then(|name| variants.iter().position(|variant| variant.name == name));
        let next = cycle(current, variants.len(), delta);
        *get_mut(&mut self.value, &row.path) = switch_variant(&current_value, tag, &variants[next]);
        self.refocus(row);
    }

    /// Enter and Space: flip a switch, add to a list, or move on.
    fn activate(&mut self, row: &Row, advance: bool) {
        match &row.op {
            Op::Toggle => {
                let on = get(&self.value, &row.path) == &Value::Bool(true);
                *get_mut(&mut self.value, &row.path) = Value::Bool(!on);
            }
            Op::Choice(options) => self.cycle_choice(row, options, 1),
            Op::Tag { node } => self.cycle_tag(row, *node, 1),
            Op::Presence { node } => {
                let target = get_mut(&mut self.value, &row.path);
                *target = if target.is_null() {
                    default_for(node, true)
                } else {
                    Value::Null
                };
                self.refocus(row);
            }
            Op::Flag(option) => {
                let target = get_mut(&mut self.value, &row.path);
                if !target.is_array() {
                    *target = json!([]);
                }
                let items = target.as_array_mut().expect("just made an array");
                if let Some(at) = items.iter().position(|item| item.as_str() == Some(option)) {
                    items.remove(at);
                } else {
                    items.push(Value::from(*option));
                }
            }
            Op::Add { node } => self.add(row, *node),
            Op::Input(_) if advance => self.step_focus(1),
            _ => {}
        }
    }

    /// After a structural change, keep the focus on the same row.
    fn refocus(&mut self, row: &Row) {
        let rows = self.rows();
        self.focus = rows
            .iter()
            .position(|candidate| {
                candidate.path == row.path
                    && std::mem::discriminant(&candidate.op) == std::mem::discriminant(&row.op)
            })
            .unwrap_or(self.focus.min(rows.len().saturating_sub(1)));
        self.load_buffer(&rows);
    }

    fn add(&mut self, row: &Row, node: Node) {
        if matches!(node, Node::Chain) {
            let chain = get_mut(&mut self.value, &row.path);
            let steps = chain
                .as_object_mut()
                .map(|chain| chain.entry("steps").or_insert_with(|| json!([])));
            if let Some(Value::Array(steps)) = steps {
                steps.push(json!({
                    "edge": default_for(&Node::Object(EDGE), true),
                    "candidate": default_for(&Node::Object(CANDIDATE), true),
                }));
            }
            self.after_add(row);
            return;
        }
        if let Node::Map(item) = node {
            let target = get_mut(&mut self.value, &row.path);
            if !target.is_object() {
                *target = json!({});
            }
            let map = target.as_object_mut().expect("just made an object");
            let name = (1..)
                .map(|n| format!("name-{n}"))
                .find(|name| !map.contains_key(name))
                .expect("an unused name exists");
            map.insert(name, default_for(item, true));
            self.after_add(row);
            return;
        }
        if let Node::List(item) = node {
            let target = get_mut(&mut self.value, &row.path);
            if !target.is_array() {
                *target = json!([]);
            }
            target
                .as_array_mut()
                .expect("just made an array")
                .push(default_for(item, true));
            self.after_add(row);
        }
    }

    /// An addition leaves the focus on the new item's first field.
    fn after_add(&mut self, row: &Row) {
        let rows = self.rows();
        let at = rows
            .iter()
            .position(|candidate| {
                candidate.path == row.path && matches!(candidate.op, Op::Add { .. })
            })
            .unwrap_or(0);
        // The new item's rows sit just above the add row.
        let first_of_new = rows[..at]
            .iter()
            .rposition(|candidate| {
                candidate
                    .item
                    .as_ref()
                    .is_some_and(|item| item.owner == row.path && item.index + 1 == item.len)
                    && matches!(candidate.op, Op::ItemHeader | Op::Input(_))
            })
            .unwrap_or(at.saturating_sub(1));
        self.focus = rows[first_of_new..at]
            .iter()
            .position(Row::focusable)
            .map_or(first_of_new, |offset| first_of_new + offset);
        self.load_buffer(&rows);
    }

    fn remove_item(&mut self, row: &Row) {
        let Some(item) = &row.item else { return };
        match &item.kind {
            ItemKind::List => {
                if let Value::Array(items) = get_mut(&mut self.value, &item.owner)
                    && item.index < items.len()
                {
                    items.remove(item.index);
                }
            }
            ItemKind::Map(name) => {
                if let Value::Object(map) = get_mut(&mut self.value, &item.owner) {
                    map.remove(name);
                }
            }
            ItemKind::Chain => {
                if item.len <= 1 {
                    self.note = Some("a route needs at least one candidate".to_owned());
                    return;
                }
                let chain = get_mut(&mut self.value, &item.owner);
                let Some(steps) = chain.get("steps").and_then(Value::as_array).cloned() else {
                    return;
                };
                let mut steps = steps;
                if item.index == 0 {
                    let promoted = steps.remove(0)["candidate"].clone();
                    chain["first"] = promoted;
                } else {
                    steps.remove(item.index - 1);
                }
                chain["steps"] = Value::Array(steps);
            }
        }
        let rows = self.rows();
        self.focus = self.focus.min(rows.len().saturating_sub(1));
        while self.focus > 0 && !rows[self.focus].focusable() {
            self.focus -= 1;
        }
        self.load_buffer(&rows);
    }

    /// Move an item one place. A route's candidates move while every edge
    /// keeps its position: the first candidate has no edge into it, so one
    /// moved to or from the front cannot carry an edge with it, and every
    /// step always keeps the edge the daemon's step record requires.
    fn move_item(&mut self, row: &Row, delta: isize) {
        let Some(item) = &row.item else { return };
        let to = item.index.saturating_add_signed(delta);
        if to >= item.len || to == item.index {
            return;
        }
        match &item.kind {
            ItemKind::List => {
                if let Value::Array(items) = get_mut(&mut self.value, &item.owner) {
                    items.swap(item.index, to);
                }
            }
            ItemKind::Map(_) => return,
            ItemKind::Chain => {
                let chain = get_mut(&mut self.value, &item.owner);
                let mut candidates = vec![chain["first"].clone()];
                let steps = chain["steps"].as_array().cloned().unwrap_or_default();
                let mut edges = Vec::new();
                for step in &steps {
                    edges.push(step["edge"].clone());
                    candidates.push(step["candidate"].clone());
                }
                candidates.swap(item.index, to);
                let mut candidates = candidates.into_iter();
                chain["first"] = candidates.next().unwrap_or(Value::Null);
                chain["steps"] = Value::Array(
                    edges
                        .into_iter()
                        .zip(candidates)
                        .map(|(edge, candidate)| json!({"edge": edge, "candidate": candidate}))
                        .collect(),
                );
            }
        }
        // Follow the moved item: its header row now sits at the other index.
        let rows = self.rows();
        let moved = rows.iter().position(|candidate| {
            candidate
                .item
                .as_ref()
                .is_some_and(|other| other.owner == item.owner && other.index == to)
                && matches!(candidate.op, Op::ItemHeader | Op::Input(_))
        });
        if let Some(moved) = moved {
            self.focus = moved;
            self.load_buffer(&rows);
        }
    }
}

/// Whether a key would change the focused field, as opposed to moving the
/// cursor or the focus.
fn edits(event: &KeyEvent, row: &Row) -> bool {
    let plain = !event
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    match event.code {
        KeyCode::Char(_) => plain,
        KeyCode::Backspace | KeyCode::Delete => true,
        KeyCode::Left | KeyCode::Right | KeyCode::Enter => !row.text_like() && plain,
        _ => false,
    }
}

fn owner_path(row: &Row) -> &[Seg] {
    row.item.as_ref().map_or(&[], |item| &item.owner)
}

fn cycle(current: Option<usize>, length: usize, delta: isize) -> usize {
    match current {
        Some(current) => (current as isize + delta).rem_euclid(length as isize) as usize,
        None => 0,
    }
}

fn byte_index(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(index, _)| index)
}

fn tag_name(node: &Node) -> Option<&'static str> {
    match node {
        Node::Union { tag, .. } => Some(tag),
        _ => None,
    }
}

fn path_text(path: &[Seg]) -> String {
    path.iter()
        .map(|seg| match seg {
            Seg::Key(name) => name.clone(),
            Seg::Index(index) => index.to_string(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn get<'a>(value: &'a Value, path: &[Seg]) -> &'a Value {
    let mut at = value;
    for seg in path {
        at = match seg {
            Seg::Key(name) => at.get(name),
            Seg::Index(index) => at.get(index),
        }
        .unwrap_or(&Value::Null);
    }
    at
}

/// The value at `path`, made if it is not there: objects and arrays are
/// grown as the path needs them.
fn get_mut<'a>(value: &'a mut Value, path: &[Seg]) -> &'a mut Value {
    let mut at = value;
    for seg in path {
        at = match seg {
            Seg::Key(name) => {
                if !at.is_object() {
                    *at = Value::Object(Map::new());
                }
                at.as_object_mut()
                    .expect("made an object")
                    .entry(name.clone())
                    .or_insert(Value::Null)
            }
            Seg::Index(index) => {
                if !at.is_array() {
                    *at = Value::Array(Vec::new());
                }
                let items = at.as_array_mut().expect("made an array");
                while items.len() <= *index {
                    items.push(Value::Null);
                }
                &mut items[*index]
            }
        };
    }
    at
}

/// The value a new instance of `node` starts at. `present` makes an optional
/// group or union exist rather than be absent.
fn default_for(node: &Node, present: bool) -> Value {
    match node {
        Node::Text | Node::Binding | Node::Model => Value::from(""),
        Node::OptText | Node::OptInt => Value::Null,
        Node::Int => Value::from(0),
        Node::Bool(on) => Value::Bool(*on),
        Node::Choice(options) => Value::from(options[0]),
        Node::Ref { optional: true, .. } => Value::Null,
        Node::Ref { .. } => Value::from(""),
        Node::Object(fields) => object_of(fields),
        Node::OptObject(fields) => {
            if present {
                object_of(fields)
            } else {
                Value::Null
            }
        }
        Node::Union {
            tag,
            variants,
            optional,
        } => {
            if *optional && !present {
                Value::Null
            } else {
                switch_variant(&Value::Null, tag, &variants[0])
            }
        }
        Node::List(_) | Node::Flags(_) => json!([]),
        Node::Map(_) => json!({}),
        Node::Chain => json!({
            "first": object_of(CANDIDATE),
            "steps": [],
        }),
    }
}

fn object_of(fields: &[Field]) -> Value {
    let mut map = Map::new();
    for field in fields {
        let value = match field.node {
            Node::Ref { optional: true, .. } | Node::OptObject(_) => Value::Null,
            ref node => default_for(node, false),
        };
        map.insert(field.key.to_owned(), value);
    }
    Value::Object(map)
}

/// The union as `variant`, keeping the fields it shares with what it was: a
/// value of the same JSON type under the same key carries over, so switching
/// a matcher from glob to regex keeps its pattern.
fn switch_variant(old: &Value, tag: &str, variant: &Variant) -> Value {
    let mut map = Map::new();
    map.insert(tag.to_owned(), Value::from(variant.name));
    for field in variant.fields {
        let fresh = match field.node {
            Node::Ref { optional: true, .. } | Node::OptObject(_) => Value::Null,
            ref node => default_for(node, false),
        };
        let kept = old.get(field.key).filter(|existing| {
            std::mem::discriminant(*existing) == std::mem::discriminant(&fresh)
                && !existing.is_null()
        });
        map.insert(field.key.to_owned(), kept.cloned().unwrap_or(fresh));
    }
    Value::Object(map)
}

fn walk(
    node: &Node,
    label: &str,
    path: Vec<Seg>,
    depth: usize,
    value: &Value,
    rows: &mut Vec<Row>,
) {
    let current = get(value, &path);
    let row = |op: Op| Row {
        path: path.clone(),
        depth,
        label: label.to_owned(),
        op,
        item: None,
    };
    match node {
        Node::Text => rows.push(row(Op::Input(Input::Text))),
        Node::OptText => rows.push(row(Op::Input(Input::OptText))),
        Node::Int => rows.push(row(Op::Input(Input::Int))),
        Node::OptInt => rows.push(row(Op::Input(Input::OptInt))),
        Node::Binding => rows.push(row(Op::Input(Input::Binding))),
        Node::Model => rows.push(row(Op::Input(Input::Model))),
        Node::Ref { kind, optional } => rows.push(row(Op::Input(Input::Ref {
            kind,
            optional: *optional,
        }))),
        Node::Bool(_) => rows.push(row(Op::Toggle)),
        Node::Choice(options) => rows.push(row(Op::Choice(options))),
        Node::Object(fields) => {
            rows.push(row(Op::Heading));
            walk_fields(fields, &path, depth + 1, value, rows);
        }
        Node::OptObject(fields) => {
            rows.push(row(Op::Presence { node: *node }));
            if !current.is_null() {
                walk_fields(fields, &path, depth + 1, value, rows);
            }
        }
        Node::Union {
            variants, optional, ..
        } => {
            if *optional {
                rows.push(row(Op::Presence { node: *node }));
                if current.is_null() {
                    return;
                }
            } else {
                rows.push(row(Op::Heading));
            }
            rows.push(Row {
                label: "kind".to_owned(),
                depth: depth + 1,
                ..row(Op::Tag { node: *node })
            });
            let tag = tag_name(node).unwrap_or("kind");
            let name = current.get(tag).and_then(Value::as_str);
            if let Some(variant) = variants.iter().find(|variant| Some(variant.name) == name) {
                walk_fields(variant.fields, &path, depth + 1, value, rows);
            }
        }
        Node::List(item) => {
            rows.push(row(Op::Heading));
            let length = current.as_array().map_or(0, Vec::len);
            for index in 0..length {
                let mut item_path = path.clone();
                item_path.push(Seg::Index(index));
                let item_info = Item {
                    owner: path.clone(),
                    index,
                    len: length,
                    kind: ItemKind::List,
                };
                walk_item(
                    item,
                    &format!("{} {}", singular_label(label), index + 1),
                    item_path,
                    depth + 1,
                    value,
                    item_info,
                    rows,
                );
            }
            rows.push(Row {
                label: singular_label(label),
                depth: depth + 1,
                ..row(Op::Add { node: *node })
            });
        }
        Node::Flags(options) => {
            rows.push(row(Op::Heading));
            for option in *options {
                rows.push(Row {
                    label: (*option).to_owned(),
                    depth: depth + 1,
                    ..row(Op::Flag(option))
                });
            }
        }
        Node::Map(item) => {
            rows.push(row(Op::Heading));
            let names: Vec<String> = current
                .as_object()
                .map(|map| map.keys().cloned().collect())
                .unwrap_or_default();
            let length = names.len();
            for (index, name) in names.into_iter().enumerate() {
                let mut item_path = path.clone();
                item_path.push(Seg::Key(name.clone()));
                rows.push(Row {
                    path: item_path.clone(),
                    depth: depth + 1,
                    label: "name".to_owned(),
                    op: Op::Input(Input::MapKey),
                    item: Some(Item {
                        owner: path.clone(),
                        index,
                        len: length,
                        kind: ItemKind::Map(name),
                    }),
                });
                // The entry's controls sit on its name row, so the value's own
                // rows carry none.
                walk(item, "value", item_path, depth + 2, value, rows);
            }
            rows.push(Row {
                label: "argument".to_owned(),
                depth: depth + 1,
                ..row(Op::Add { node: *node })
            });
        }
        Node::Chain => walk_chain(&path, depth, value, rows),
    }
}

fn walk_fields(fields: &[Field], path: &[Seg], depth: usize, value: &Value, rows: &mut Vec<Row>) {
    for field in fields {
        let mut field_path = path.to_vec();
        field_path.push(key(field.key));
        walk(&field.node, field.label, field_path, depth, value, rows);
    }
}

/// A list or map item. A scalar item is one row that carries the item's
/// controls; a compound one gets a header row that does.
fn walk_item(
    node: &Node,
    label: &str,
    path: Vec<Seg>,
    depth: usize,
    value: &Value,
    item: Item,
    rows: &mut Vec<Row>,
) {
    let scalar = matches!(
        node,
        Node::Text
            | Node::OptText
            | Node::Int
            | Node::OptInt
            | Node::Binding
            | Node::Model
            | Node::Ref { .. }
            | Node::Bool(_)
            | Node::Choice(_)
    );
    if scalar {
        let before = rows.len();
        walk(node, label, path, depth, value, rows);
        if let Some(row) = rows.get_mut(before) {
            row.item = Some(item);
        }
        return;
    }
    rows.push(Row {
        path: path.clone(),
        depth,
        label: label.to_owned(),
        op: Op::ItemHeader,
        item: Some(item),
    });
    // A compound item's own heading would repeat its header.
    let before = rows.len();
    walk(node, "", path, depth + 1, value, rows);
    if rows
        .get(before)
        .is_some_and(|row| matches!(row.op, Op::Heading) && row.label.is_empty())
    {
        rows.remove(before);
    }
}

/// A route's candidates: each candidate's header and fields, with the edge
/// into every candidate after the first between them.
fn walk_chain(path: &[Seg], depth: usize, value: &Value, rows: &mut Vec<Row>) {
    let chain = get(value, path);
    let steps = chain
        .get("steps")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let length = 1 + steps;
    rows.push(Row {
        path: path.to_vec(),
        depth,
        label: "candidates".to_owned(),
        op: Op::Heading,
        item: None,
    });
    for index in 0..length {
        let mut base = path.to_vec();
        let candidate_path = if index == 0 {
            base.push(key("first"));
            base
        } else {
            base.push(key("steps"));
            base.push(Seg::Index(index - 1));
            base.push(key("candidate"));
            base
        };
        if index > 0 {
            let mut edge_path = path.to_vec();
            edge_path.push(key("steps"));
            edge_path.push(Seg::Index(index - 1));
            edge_path.push(key("edge"));
            rows.push(Row {
                path: edge_path.clone(),
                depth: depth + 1,
                label: format!("fallback from candidate {} to {}", index, index + 1),
                op: Op::Heading,
                item: None,
            });
            walk_fields(EDGE, &edge_path, depth + 2, value, rows);
        }
        rows.push(Row {
            path: candidate_path.clone(),
            depth: depth + 1,
            label: format!("candidate {}", index + 1),
            op: Op::ItemHeader,
            item: Some(Item {
                owner: path.to_vec(),
                index,
                len: length,
                kind: ItemKind::Chain,
            }),
        });
        walk_fields(CANDIDATE, &candidate_path, depth + 2, value, rows);
    }
    rows.push(Row {
        path: path.to_vec(),
        depth: depth + 1,
        label: "candidate".to_owned(),
        op: Op::Add { node: Node::Chain },
        item: None,
    });
}

/// "health circuits" is a list of "health circuit".
fn singular_label(label: &str) -> String {
    let label = label.trim();
    if let Some(stem) = label.strip_suffix("ies") {
        format!("{stem}y")
    } else if let Some(stem) = label.strip_suffix('s') {
        stem.to_owned()
    } else {
        label.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::tui::routing::template::template;

    fn press(form: &mut Form, code: KeyCode) -> Outcome {
        form.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(form: &mut Form, text: &str) {
        for character in text.chars() {
            press(form, KeyCode::Char(character));
        }
    }

    fn focus_label(form: &mut Form, label: &str) {
        let rows = form.rows();
        let at = rows
            .iter()
            .position(|row| row.focusable() && row.label == label)
            .unwrap_or_else(|| panic!("no field labelled {label:?}"));
        form.set_focus(at);
    }

    fn record(kind: &'static str) -> Form {
        Form::new(
            Purpose::Record { kind, adding: true },
            template(kind, Some("route-id")),
            Context::default(),
        )
    }

    /// Saving a form that was not edited gives back the template's record,
    /// whole: nothing the schema walks adds, drops or reorders a field the
    /// daemon takes.
    #[test]
    fn an_unedited_form_saves_the_record_it_was_given() {
        for kind in [
            "presentations",
            "routes",
            "defaults",
            "rewrites",
            "classifiers",
            "upgrades",
            "compaction",
        ] {
            let form = record(kind);
            assert_eq!(*form.value(), template(kind, Some("route-id")), "{kind}");
        }
    }

    /// Every key a template carries has a field in the form, so a field the
    /// daemon takes cannot be missing from what the form shows.
    #[test]
    fn every_key_of_every_template_has_a_field() {
        for kind in [
            "presentations",
            "routes",
            "defaults",
            "rewrites",
            "classifiers",
            "upgrades",
            "compaction",
        ] {
            let form = record(kind);
            let rows = form.rows();
            for name in form.value().as_object().expect("an object").keys() {
                assert!(
                    rows.iter().any(|row| row.path.first() == Some(&key(name))),
                    "{kind}: no field for {name}"
                );
            }
        }
    }

    #[test]
    fn every_kind_has_fields_to_edit() {
        for kind in [
            "presentations",
            "routes",
            "defaults",
            "rewrites",
            "classifiers",
            "upgrades",
            "compaction",
        ] {
            assert!(
                record(kind).rows().iter().any(Row::focusable),
                "{kind} has no field"
            );
        }
    }

    #[test]
    fn typing_edits_the_focused_text_and_the_cursor_moves() {
        let mut form = record("rewrites");
        focus_label(&mut form, "id");
        // The cursor starts at the end of "new-rewrite".
        type_text(&mut form, "-x");
        assert_eq!(form.value()["id"], "new-rewrite-x");
        press(&mut form, KeyCode::Home);
        press(&mut form, KeyCode::Delete);
        assert_eq!(form.value()["id"], "ew-rewrite-x");
        press(&mut form, KeyCode::End);
        press(&mut form, KeyCode::Backspace);
        assert_eq!(form.value()["id"], "ew-rewrite-");
    }

    #[test]
    fn a_number_accepts_digits_and_an_optional_count_can_be_cleared() {
        let mut form = record("rewrites");
        focus_label(&mut form, "priority");
        type_text(&mut form, "ab7");
        assert_eq!(form.value()["priority"], 7);
        press(&mut form, KeyCode::Backspace);
        assert_eq!(form.value()["priority"], 0);

        let mut preview = Form::new(
            Purpose::Preview,
            json!({"protocol": "openai_chat", "message_count": 3}),
            Context::default(),
        );
        focus_label(&mut preview, "message count");
        press(&mut preview, KeyCode::Backspace);
        assert_eq!(preview.value()["message_count"], Value::Null);
    }

    #[test]
    fn a_choice_cycles_through_the_values_the_daemon_accepts() {
        let mut form = record("routes");
        focus_label(&mut form, "lane transition");
        assert_eq!(form.value()["lane_transition"], "move_when_cold");
        press(&mut form, KeyCode::Right);
        assert_eq!(form.value()["lane_transition"], "allow_immediate");
        press(&mut form, KeyCode::Right);
        assert_eq!(form.value()["lane_transition"], "preserve_pinned");
        press(&mut form, KeyCode::Left);
        assert_eq!(form.value()["lane_transition"], "allow_immediate");
    }

    /// Switching a union's kind rebuilds its fields and keeps the ones the
    /// two kinds share.
    #[test]
    fn switching_a_matcher_kind_keeps_the_pattern() {
        let mut form = record("rewrites");
        let rows = form.rows();
        let kind = rows
            .iter()
            .position(|row| {
                matches!(&row.op, Op::Tag { .. }) && row.path == vec![key("model_matcher")]
            })
            .expect("a matcher kind row");
        form.set_focus(kind);
        assert_eq!(form.value()["model_matcher"]["kind"], "exact");
        press(&mut form, KeyCode::Right);
        assert_eq!(
            form.value()["model_matcher"],
            json!({"kind": "glob", "value": "model-name"})
        );
        press(&mut form, KeyCode::Right);
        assert_eq!(form.value()["model_matcher"]["kind"], "regex");
        assert_eq!(form.value()["model_matcher"]["value"], "model-name");
    }

    #[test]
    fn switching_a_presentation_source_gives_the_new_kinds_fields() {
        let mut form = record("presentations");
        let rows = form.rows();
        let kind = rows
            .iter()
            .position(|row| matches!(&row.op, Op::Tag { .. }) && row.path == vec![key("source")])
            .expect("a source kind row");
        form.set_focus(kind);
        press(&mut form, KeyCode::Right);
        assert_eq!(form.value()["source"]["kind"], "offer_selector");
        assert!(form.value()["source"]["selector"]["ordering"].is_object());
        press(&mut form, KeyCode::Right);
        assert_eq!(
            form.value()["source"],
            json!({"kind": "virtual", "target": {"route_id": "", "parameters": {}}})
        );
        // Back to an exact offer: its binding and model start empty again.
        press(&mut form, KeyCode::Right);
        assert_eq!(form.value()["source"]["kind"], "exact_offer");
        assert_eq!(form.value()["source"]["model_id"], "");
    }

    #[test]
    fn flags_toggle_a_failure_in_and_out_of_a_list() {
        let mut form = record("routes");
        press(&mut form, KeyCode::Down);
        // Add a second candidate, which brings an edge with its flags.
        focus_label(&mut form, "candidate");
        press(&mut form, KeyCode::Enter);
        focus_label(&mut form, "rate_limit");
        press(&mut form, KeyCode::Char(' '));
        assert_eq!(
            form.value()["chain"]["steps"][0]["edge"]["post_send"],
            json!(["rate_limit"])
        );
        press(&mut form, KeyCode::Char(' '));
        assert_eq!(
            form.value()["chain"]["steps"][0]["edge"]["post_send"],
            json!([])
        );
    }

    #[test]
    fn candidates_are_added_removed_and_reordered_with_edges_in_place() {
        let mut form = record("routes");
        focus_label(&mut form, "candidate");
        press(&mut form, KeyCode::Enter);
        // Adding leaves the focus on the new candidate, so go back to add.
        focus_label(&mut form, "candidate");
        press(&mut form, KeyCode::Enter);
        assert_eq!(form.value()["chain"]["steps"].as_array().unwrap().len(), 2);
        // Name the three candidates a, b, c and give the edges distinct
        // consent so a moved edge would show.
        for (index, name) in ["a", "b", "c"].into_iter().enumerate() {
            let path = match index {
                0 => vec![key("chain"), key("first"), key("id")],
                n => vec![
                    key("chain"),
                    key("steps"),
                    Seg::Index(n - 1),
                    key("candidate"),
                    key("id"),
                ],
            };
            *get_mut(&mut form.value, &path) = Value::from(name);
        }
        form.value["chain"]["steps"][0]["edge"]["billed_boundary"] = Value::from("allow");

        // Move candidate b (index 1) to the front.
        let rows = form.rows();
        let header = rows
            .iter()
            .position(|row| matches!(row.op, Op::ItemHeader) && row.label == "candidate 2")
            .unwrap();
        form.set_focus(header);
        form.key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
        assert_eq!(form.value()["chain"]["first"]["id"], "b");
        assert_eq!(form.value()["chain"]["steps"][0]["candidate"]["id"], "a");
        assert_eq!(form.value()["chain"]["steps"][1]["candidate"]["id"], "c");
        // The edge into the second slot stayed in the second slot.
        assert_eq!(
            form.value()["chain"]["steps"][0]["edge"]["billed_boundary"],
            "allow"
        );

        // Removing the first promotes the next.
        let rows = form.rows();
        let header = rows
            .iter()
            .position(|row| matches!(row.op, Op::ItemHeader) && row.label == "candidate 1")
            .unwrap();
        form.set_focus(header);
        form.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(form.value()["chain"]["first"]["id"], "a");
        assert_eq!(form.value()["chain"]["steps"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn the_last_candidate_cannot_be_removed() {
        let mut form = record("routes");
        let rows = form.rows();
        let header = rows
            .iter()
            .position(|row| matches!(row.op, Op::ItemHeader))
            .unwrap();
        form.set_focus(header);
        form.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(form.value()["chain"]["first"]["id"], "new-candidate");
        assert!(
            form.note()
                .is_some_and(|note| note.contains("at least one"))
        );
    }

    fn offered() -> Context {
        Context {
            bindings: vec![
                "anthropic_sub.anthropic_messages".to_owned(),
                "codex_sub.codex_responses".to_owned(),
            ],
            offers: vec![
                ("codex_sub.codex_responses".to_owned(), "gpt-one".to_owned()),
                ("codex_sub.codex_responses".to_owned(), "gpt-two".to_owned()),
                (
                    "anthropic_sub.anthropic_messages".to_owned(),
                    "claude-x".to_owned(),
                ),
            ],
            ..Context::default()
        }
    }

    #[test]
    fn a_binding_completes_by_search_and_cycles_with_tab() {
        let mut form = Form::new(
            Purpose::Record {
                kind: "classifiers",
                adding: true,
            },
            template("classifiers", None),
            offered(),
        );
        focus_label(&mut form, "binding");
        for _ in 0..40 {
            press(&mut form, KeyCode::Backspace);
        }
        type_text(&mut form, "codex");
        assert!(
            form.lines()
                .iter()
                .any(|line| line.text.trim() == "Tab: codex_sub.codex_responses"),
            "{:?}",
            form.lines()
        );
        press(&mut form, KeyCode::Tab);
        assert_eq!(form.value()["binding_id"], "codex_sub.codex_responses");
        // A second Tab wraps within the matches rather than moving on.
        press(&mut form, KeyCode::Tab);
        assert_eq!(form.value()["binding_id"], "codex_sub.codex_responses");
    }

    #[test]
    fn a_model_completes_against_its_own_bindings_offers() {
        let mut form = Form::new(
            Purpose::Record {
                kind: "presentations",
                adding: true,
            },
            json!({
                "id": "p", "slug": "s", "enabled": true,
                "scope": {"kind": "protocol", "protocol": "openai_responses"},
                "source": {"kind": "exact_offer",
                           "binding_id": "codex_sub.codex_responses", "model_id": ""},
            }),
            offered(),
        );
        focus_label(&mut form, "model");
        assert!(
            form.lines()
                .iter()
                .any(|line| line.text.trim() == "Tab: gpt-one | gpt-two"),
            "{:?}",
            form.lines()
        );
        type_text(&mut form, "two");
        press(&mut form, KeyCode::Tab);
        assert_eq!(form.value()["source"]["model_id"], "gpt-two");
    }

    /// A value the catalogues do not offer is kept as typed and marked, so a
    /// model that is not fetched yet can still be named.
    #[test]
    fn a_typed_value_that_is_not_offered_is_kept_and_marked() {
        let mut form = Form::new(
            Purpose::Record {
                kind: "presentations",
                adding: true,
            },
            json!({
                "id": "p", "slug": "s", "enabled": true,
                "scope": {"kind": "protocol", "protocol": "openai_responses"},
                "source": {"kind": "exact_offer",
                           "binding_id": "codex_sub.codex_responses", "model_id": ""},
            }),
            offered(),
        );
        focus_label(&mut form, "model");
        type_text(&mut form, "gpt-three");
        press(&mut form, KeyCode::Down);
        assert_eq!(form.value()["source"]["model_id"], "gpt-three");
        let shown = form
            .lines()
            .into_iter()
            .find(|line| line.text.contains("gpt-three"))
            .expect("the model row");
        assert!(shown.text.contains("(not offered)"), "{}", shown.text);
    }

    #[test]
    fn a_record_id_completes_from_the_drafts_ids() {
        let mut context = Context::default();
        context
            .ids
            .insert("routes", vec!["fast".to_owned(), "slow".to_owned()]);
        let mut form = Form::new(
            Purpose::Record {
                kind: "defaults",
                adding: true,
            },
            template("defaults", None),
            context,
        );
        focus_label(&mut form, "route");
        for _ in 0..10 {
            press(&mut form, KeyCode::Backspace);
        }
        type_text(&mut form, "sl");
        press(&mut form, KeyCode::Tab);
        assert_eq!(form.value()["target"]["route_id"], "slow");
    }

    #[test]
    fn a_map_of_arguments_adds_renames_and_removes_entries() {
        let mut form = record("defaults");
        focus_label(&mut form, "argument");
        press(&mut form, KeyCode::Enter);
        assert_eq!(
            form.value()["target"]["parameters"],
            json!({"name-1": {"kind": "literal", "value": ""}})
        );
        // The new entry's name is focused: rename it.
        for _ in 0..6 {
            press(&mut form, KeyCode::Backspace);
        }
        type_text(&mut form, "tier");
        assert_eq!(
            form.value()["target"]["parameters"],
            json!({"tier": {"kind": "literal", "value": ""}})
        );
        form.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(form.value()["target"]["parameters"], json!({}));
    }

    #[test]
    fn an_optional_group_is_switched_on_and_off() {
        let mut form = record("presentations");
        let rows = form.rows();
        let presence = rows
            .iter()
            .position(|row| {
                matches!(&row.op, Op::Presence { .. }) && row.path == vec![key("metadata")]
            })
            .expect("a metadata row");
        form.set_focus(presence);
        assert_eq!(form.value()["metadata"], Value::Null);
        press(&mut form, KeyCode::Char(' '));
        assert_eq!(form.value()["metadata"]["adapter"], "claude");
        press(&mut form, KeyCode::Char(' '));
        assert_eq!(form.value()["metadata"], Value::Null);
    }

    #[test]
    fn keys_the_schema_does_not_name_survive_an_edit() {
        let mut value = template("classifiers", None);
        value["legacy_hyphenated"] = Value::Bool(true);
        let mut form = Form::new(
            Purpose::Record {
                kind: "classifiers",
                adding: false,
            },
            value,
            Context::default(),
        );
        focus_label(&mut form, "id");
        type_text(&mut form, "x");
        assert_eq!(form.value()["legacy_hyphenated"], true);
    }

    #[test]
    fn the_id_of_an_existing_record_is_fixed() {
        let mut form = Form::new(
            Purpose::Record {
                kind: "rewrites",
                adding: false,
            },
            template("rewrites", Some("route-id")),
            Context::default(),
        );
        focus_label(&mut form, "id");
        type_text(&mut form, "x");
        assert_eq!(form.value()["id"], "new-rewrite");
        assert!(form.note().is_some_and(|note| note.contains("fixed")));
        press(&mut form, KeyCode::Backspace);
        assert_eq!(form.value()["id"], "new-rewrite");
        // Moving the cursor is not an edit.
        press(&mut form, KeyCode::Left);
        assert!(form.note().is_none());
        // A default is identified by its scope, so that is fixed too.
        let mut default = Form::new(
            Purpose::Record {
                kind: "defaults",
                adding: false,
            },
            template("defaults", None),
            Context::default(),
        );
        focus_label(&mut default, "kind");
        press(&mut default, KeyCode::Right);
        assert_eq!(default.value()["scope"]["kind"], "protocol");
        // The route it points at is not part of its identity.
        focus_label(&mut default, "route");
        type_text(&mut default, "-2");
        assert_eq!(default.value()["target"]["route_id"], "route-id-2");
    }

    #[test]
    fn a_blank_id_is_not_submitted() {
        let mut form = record("rewrites");
        focus_label(&mut form, "id");
        for _ in 0..20 {
            press(&mut form, KeyCode::Backspace);
        }
        let outcome = form.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert_eq!(outcome, Outcome::Keep);
        assert_eq!(form.note(), Some("id is empty"));
        type_text(&mut form, "ok");
        let outcome = form.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert_eq!(outcome, Outcome::Submit);
    }

    #[test]
    fn daemon_diagnostics_mark_the_field_they_name() {
        let context = Context {
            diagnostics: vec![(
                "chain/first/binding_id".to_owned(),
                "binding-unconfigured".to_owned(),
            )],
            ..Context::default()
        };
        let form = Form::new(
            Purpose::Record {
                kind: "routes",
                adding: false,
            },
            template("routes", None),
            context,
        );
        let marked: Vec<_> = form
            .lines()
            .into_iter()
            .filter(|line| line.text.contains("! binding-unconfigured"))
            .collect();
        assert_eq!(marked.len(), 1);
        assert!(marked[0].text.contains("binding"));
    }

    #[test]
    fn short_forms_submit_on_enter_and_record_forms_move_on() {
        let mut promote = Form::new(Purpose::Promote, json!({}), offered());
        assert_eq!(press(&mut promote, KeyCode::Enter), Outcome::Submit);
        let mut record = record("rewrites");
        focus_label(&mut record, "id");
        assert_eq!(press(&mut record, KeyCode::Enter), Outcome::Keep);
        assert_eq!(record.rows()[record.focus()].label, "kind");
    }
}
