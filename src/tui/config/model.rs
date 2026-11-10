//! The configuration pages' state: what the snapshot says, what is
//! selected, the form open under a row, and the command an edit turns into.
//!
//! The service is the only writer, so every change here is a request and
//! nothing is applied locally: the next snapshot is what the page believes
//! (spec: DCONF#editing). A refused value leaves the row as it was and is
//! reported beside the field that held it, which falls out of never writing
//! ahead of the reply.

use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use reqwest::Method;
use serde_json::{Value, json};

use super::check;
use crate::tui::control::{Command, Reply, Response};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum View {
    #[default]
    Providers,
    Settings,
}

/// One provider as the snapshot describes it. Credentials are presence
/// only: no operation returns a value
/// (spec: SEC#a-credential-is-never-logged-or-displayed).
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Provider {
    pub(super) name: String,
    pub(super) enabled: bool,
    pub(super) upstream: Option<String>,
    pub(super) cred_env: Option<String>,
    pub(super) env_set: bool,
    pub(super) keyring: bool,
    pub(super) literal_set: bool,
    /// Settings of the codex provider's login handshake; every edit sends
    /// them back whole, since the service writes the row it is given.
    pub(super) refresh_url: Option<String>,
    pub(super) client_version: Option<String>,
    pub(super) version_probe: Option<bool>,
}

impl Provider {
    /// Whether a credential resolves at all. A provider with none is not
    /// configured, which is the answer a request reaching it gets
    /// (spec: UTUI#providers).
    pub(super) fn has_credential(&self) -> bool {
        self.env_set || self.keyring || self.literal_set
    }

    /// Codex signs with the login it shares with the codex CLI, so toker
    /// holds no credential of its own for it.
    pub(super) fn holds_a_credential(&self) -> bool {
        self.name != "codex_sub"
    }

    /// Enabled, and with no credential to answer with.
    pub(super) fn is_not_configured(&self) -> bool {
        self.enabled && self.holds_a_credential() && !self.has_credential()
    }

    /// Whether the row carries the login handshake's settings, which only
    /// the codex provider reads.
    pub(super) fn has_handshake_settings(&self) -> bool {
        self.name == "codex_sub"
    }

    /// Whether the client version is asked of the service, which is the
    /// default and which a pinned version switches off.
    pub(super) fn probes_version(&self) -> bool {
        self.version_probe.unwrap_or(true)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Settings {
    pub(super) quota_enabled: Option<bool>,
    pub(super) cold_enabled: Option<bool>,
    pub(super) cold_outlook: Option<bool>,
    pub(super) cold_min_tokens: Option<u64>,
    pub(super) cold_idle_min: Option<f64>,
    pub(super) awake: Option<bool>,
    /// Not shown on any page yet, but carried back whole: the settings
    /// edit replaces the row, so a page that dropped it would turn the
    /// schedule off with every other settings change.
    pub(super) ping_schedule: Option<bool>,
    pub(super) ping_header: Option<String>,
    pub(super) notice_default: Option<String>,
    pub(super) session_headers: Option<Vec<String>>,
    pub(super) transcript_roots: Option<Vec<String>>,
}

/// One Messages binding's standing with server safeguards: what it declares
/// and whether a refusal has marked it. Support words only, never anything a
/// request carried (spec: WSAFE#what-is-recorded).
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Safeguards {
    pub(super) binding_id: String,
    pub(super) provider: String,
    /// `supported`, `unsupported` or `unknown`, as the service says it.
    pub(super) declared: String,
    /// When a rejection mark was set against the binding, if one is held.
    pub(super) marked_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Snapshot {
    pub(super) providers: Vec<Provider>,
    pub(super) safeguards: Vec<Safeguards>,
    pub(super) settings: Settings,
    /// The notice style set for each frontend profile.
    pub(super) notices: BTreeMap<String, String>,
    /// The largest context window the enabled providers' models resolve,
    /// which a cold-gate minimum is checked against; absent where none
    /// does.
    pub(super) largest_context_window: Option<u64>,
    /// The daemon's word that the cold-gate minimum in force is larger than
    /// every window those models resolve, so no request could reach it.
    pub(super) cold_min_unreachable: bool,
    pub(super) db_path: String,
    pub(super) port: u64,
}

/// The frontend profiles whose notice style is always offered, ahead of any
/// others the store holds a style for.
const PROFILES: [&str; 3] = ["claude", "opencode", "codex"];

impl Snapshot {
    /// The profiles the settings page lists a notice style for: the ones
    /// Toker ships a client setup for and any it has a built-in style for,
    /// then any the operator has set a style on.
    pub(super) fn profiles(&self) -> Vec<String> {
        let built_in = crate::config::NoticesConfig::default().frontends;
        let mut profiles: Vec<String> = PROFILES.iter().map(|name| (*name).to_owned()).collect();
        for name in built_in.keys().chain(self.notices.keys()) {
            if name != "default" && !profiles.contains(name) {
                profiles.push(name.clone());
            }
        }
        profiles[PROFILES.len()..].sort();
        profiles
    }

    /// The style a profile gets with nothing set for it.
    pub(super) fn notice_fallback(&self, profile: &str) -> String {
        match crate::config::NoticesConfig::default()
            .frontends
            .get(profile)
        {
            Some(style) => style.name().to_owned(),
            None => self.default_notice(),
        }
    }

    /// The style a profile with none of its own gets.
    pub(super) fn default_notice(&self) -> String {
        self.settings.notice_default.clone().unwrap_or_else(|| {
            crate::config::NoticesConfig::default()
                .default
                .name()
                .to_owned()
        })
    }
}

/// What in routing reaches one provider, as the routing snapshot derives it
/// (spec: UTUI#providers).
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Dependents {
    pub(super) provider: String,
    /// Each endpoint the provider has, with how it bills.
    pub(super) bindings: Vec<(String, String)>,
    pub(super) presentations: usize,
    pub(super) rewrites: usize,
    pub(super) rewrites_by_protocol: Vec<(String, usize)>,
    pub(super) defaults: Vec<String>,
    pub(super) routes: Vec<String>,
}

/// One row of the page: what it names and what acting on it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Row {
    /// A provider, by its place in the snapshot.
    Provider(usize),
    Quota,
    Cold,
    ColdOutlook,
    ColdMinTokens,
    ColdIdleMin,
    SessionHeaders,
    PingHeader,
    Awake,
    TranscriptRoots,
    NoticeDefault,
    /// One profile's notice style, by its place in [`Snapshot::profiles`].
    Notice(usize),
    /// Shown so the page can report it, never offered for editing
    /// (spec: UTUI#settings).
    Port,
    Store,
}

impl Row {
    /// Whether acting on this row toggles it rather than opening an editor.
    pub(super) fn is_toggle(self) -> bool {
        matches!(
            self,
            Row::Provider(_) | Row::Quota | Row::Cold | Row::ColdOutlook | Row::Awake
        )
    }

    /// Whether the row can be changed at all.
    pub(super) fn is_editable(self) -> bool {
        !matches!(self, Row::Port | Row::Store)
    }
}

/// What a form in progress will change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Target {
    Endpoint(usize),
    Credential(usize),
    RefreshUrl(usize),
    ClientVersion(usize),
    Setting(Row),
}

impl Target {
    /// The name the form gives its field.
    pub(super) fn field(self) -> &'static str {
        match self {
            Target::Endpoint(_) => "endpoint",
            Target::Credential(_) => "key",
            Target::RefreshUrl(_) => "refresh url",
            Target::ClientVersion(_) => "version",
            Target::Setting(Row::ColdMinTokens) => "tokens",
            Target::Setting(Row::ColdIdleMin) => "minutes",
            Target::Setting(Row::SessionHeaders) => "names",
            Target::Setting(Row::PingHeader) => "name",
            Target::Setting(Row::TranscriptRoots) => "paths",
            Target::Setting(_) => "style",
        }
    }
}

/// A form in progress: what it will change, and what has been typed. A
/// credential entry is never echoed back by the page.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Editing {
    pub(super) target: Target,
    pub(super) buffer: String,
    pub(super) secret: bool,
    /// What the service said of the value it was sent, kept beside the field
    /// until the value changes (spec: UTUI#the-configuration-pages).
    pub(super) refusal: Option<String>,
    /// A save is with the service, which has not answered yet.
    sent: bool,
}

/// A value as the form's check reads it: what would be stored.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Change {
    Text(Option<String>),
    Tokens(Option<u64>),
    Minutes(Option<f64>),
    List(Option<Vec<String>>),
}

/// What the form's check makes of what has been typed: the refusal to show
/// beside the field, or a reading of what it will do.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Verdict {
    Accepted(Option<String>),
    Refused(String),
}

/// Judge `buffer` as the value of `target`, from what the text and the
/// snapshot can tell; the service still has the last word on save.
pub(super) fn judge(
    target: Target,
    buffer: &str,
    snapshot: &Snapshot,
) -> Result<(Change, Option<String>), String> {
    let typed = buffer.trim();
    let empty = typed.is_empty();
    let said = |words: String| (!empty).then_some(words);
    let note = |words: String| Some(words);
    match target {
        Target::Endpoint(_) => text(
            empty,
            typed,
            check::parse_url,
            |_| None,
            || note("empty uses the built-in endpoint".to_owned()),
        ),
        Target::RefreshUrl(_) => text(
            empty,
            typed,
            check::parse_url,
            |_| None,
            || note("empty uses the built-in address".to_owned()),
        ),
        Target::ClientVersion(_) => text(
            empty,
            typed,
            check::parse_version,
            |_| None,
            || note("empty pins no version".to_owned()),
        ),
        Target::Credential(_) => Ok((
            Change::Text((!empty).then(|| typed.to_owned())),
            empty.then(|| "empty clears the stored key".to_owned()),
        )),
        Target::Setting(Row::ColdMinTokens) => {
            if empty {
                return Ok((
                    Change::Tokens(None),
                    note(format!(
                        "empty uses the built-in {}",
                        check::tokens_shown(crate::middleware::cold::DEFAULT_MIN_TOKENS)
                    )),
                ));
            }
            let tokens = check::parse_tokens(typed)?;
            if let Some(largest) = snapshot.largest_context_window
                && tokens > largest
            {
                let largest = check::tokens_shown(largest);
                return Err(format!(
                    "more than any configured model's context, {largest}: use {largest} or less"
                ));
            }
            Ok((
                Change::Tokens(Some(tokens)),
                said(format!("{} tokens", check::group(tokens))),
            ))
        }
        Target::Setting(Row::ColdIdleMin) => {
            if empty {
                return Ok((
                    Change::Minutes(None),
                    note("empty follows the lane's TTL tier".to_owned()),
                ));
            }
            let minutes = check::parse_minutes(typed)?;
            Ok((Change::Minutes(Some(minutes)), None))
        }
        Target::Setting(Row::SessionHeaders) => list(empty, typed, check::parse_headers, || {
            "empty uses the built-in names".to_owned()
        }),
        Target::Setting(Row::TranscriptRoots) => list(empty, typed, check::parse_paths, || {
            "empty adds no directory".to_owned()
        }),
        Target::Setting(Row::PingHeader) => text(
            empty,
            typed,
            check::parse_header,
            |_| None,
            || {
                note(format!(
                    "empty uses the built-in {}",
                    crate::config::DEFAULT_PING_HEADER
                ))
            },
        ),
        Target::Setting(Row::NoticeDefault) => text(
            empty,
            typed,
            check::parse_style,
            |_| None,
            || {
                note(format!(
                    "empty uses the built-in {}",
                    crate::config::NoticesConfig::default().default.name()
                ))
            },
        ),
        Target::Setting(Row::Notice(at)) => {
            let profile = snapshot.profiles().get(at).cloned().unwrap_or_default();
            let fallback = if crate::config::NoticesConfig::default()
                .frontends
                .contains_key(&profile)
            {
                format!(
                    "empty uses the built-in {}",
                    snapshot.notice_fallback(&profile)
                )
            } else {
                format!("empty follows the default, {}", snapshot.default_notice())
            };
            text(
                empty,
                typed,
                check::parse_style,
                |_| None,
                || note(fallback),
            )
        }
        Target::Setting(_) => Err("this is not edited as text".to_owned()),
    }
}

fn text(
    empty: bool,
    typed: &str,
    parse: fn(&str) -> Result<String, String>,
    reading: impl Fn(&str) -> Option<String>,
    fallback: impl FnOnce() -> Option<String>,
) -> Result<(Change, Option<String>), String> {
    if empty {
        return Ok((Change::Text(None), fallback()));
    }
    let value = parse(typed)?;
    let said = reading(&value);
    Ok((Change::Text(Some(value)), said))
}

fn list(
    empty: bool,
    typed: &str,
    parse: fn(&str) -> Result<Vec<String>, String>,
    fallback: impl FnOnce() -> String,
) -> Result<(Change, Option<String>), String> {
    if empty {
        return Ok((Change::List(None), Some(fallback())));
    }
    let names = parse(typed)?;
    let count = names.len();
    Ok((
        Change::List(Some(names)),
        Some(format!(
            "{count} {}",
            if count == 1 { "entry" } else { "entries" }
        )),
    ))
}

impl Editing {
    /// Whether the save is with the service, which has not answered yet.
    pub(super) fn is_sent(&self) -> bool {
        self.sent
    }

    /// What the check makes of the buffer as it stands.
    pub(super) fn verdict(&self, snapshot: &Snapshot) -> Verdict {
        match judge(self.target, &self.buffer, snapshot) {
            Ok((_, reading)) => Verdict::Accepted(reading),
            Err(refusal) => Verdict::Refused(refusal),
        }
    }

    /// Whether the buffer can be saved as it stands: nothing is refused,
    /// here or by the service for this very value.
    pub(super) fn saveable(&self, snapshot: &Snapshot) -> bool {
        self.refusal.is_none() && judge(self.target, &self.buffer, snapshot).is_ok()
    }
}

#[derive(Debug, Default)]
pub(super) struct State {
    view: View,
    snapshot: Option<Snapshot>,
    /// What routing depends on each provider; unknown until the routing
    /// snapshot has been read, and where it could not be.
    dependents: Option<Vec<Dependents>>,
    selected: usize,
    error: Option<String>,
    in_flight: bool,
    /// The service did not answer the last read. The pages then report that
    /// rather than offer values to edit (spec: UTUI#the-configuration-pages).
    unreachable: bool,
    editing: Option<Editing>,
    /// Bindings whose marks are still to be cleared after the one in flight.
    /// A provider can hold several Messages bindings and the operation takes
    /// one, so the replies chain them.
    pending_clears: Vec<String>,
    /// Whether the providers page has opened on its problem yet, which it
    /// does once per arrival (spec: UTUI#providers).
    opened: bool,
    /// A route the operator asked to be taken to, for the loop to act on.
    goto: Option<String>,
}

impl State {
    pub(super) fn view(&self) -> View {
        self.view
    }

    pub(super) fn set_view(&mut self, view: View) {
        if self.view != view {
            self.view = view;
            self.selected = 0;
        }
    }

    pub(super) fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(super) fn editing(&self) -> Option<&Editing> {
        self.editing.as_ref()
    }

    pub(super) fn selected(&self) -> usize {
        self.selected
    }

    /// Whether the page can offer anything to edit. A page with no reachable
    /// service is a page whose edits would go nowhere.
    pub(super) fn reachable(&self) -> bool {
        !self.unreachable && self.snapshot.is_some()
    }

    /// The row the cursor is on.
    pub(super) fn row(&self) -> Option<Row> {
        self.rows().get(self.selected).copied()
    }

    /// What routing depends on `provider`: `None` where that is not known,
    /// and nothing where the routing snapshot read does not mention it.
    pub(super) fn dependents(&self, provider: &str) -> Option<Dependents> {
        let all = self.dependents.as_ref()?;
        Some(
            all.iter()
                .find(|found| found.provider == provider)
                .cloned()
                .unwrap_or_else(|| Dependents {
                    provider: provider.to_owned(),
                    ..Dependents::default()
                }),
        )
    }

    /// A refusal, as the service's reply to a write leaves one.
    #[cfg(test)]
    pub(super) fn set_error(&mut self, error: &str) {
        self.error = Some(error.to_owned());
    }

    /// Open on the problem again, the way arriving at the page does.
    pub(super) fn reopen(&mut self) {
        self.opened = false;
    }

    /// The route the operator asked to be taken to, once.
    pub(super) fn take_goto(&mut self) -> Option<String> {
        self.goto.take()
    }

    /// The rows the current view lists, in order.
    pub(super) fn rows(&self) -> Vec<Row> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        match self.view {
            View::Providers => (0..snapshot.providers.len()).map(Row::Provider).collect(),
            View::Settings => self
                .groups()
                .into_iter()
                .flat_map(|(_, rows)| rows)
                .collect(),
        }
    }

    /// The settings, grouped as the page draws them.
    pub(super) fn groups(&self) -> Vec<(&'static str, Vec<Row>)> {
        let notices = self
            .snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.profiles().len());
        vec![
            (
                "gates",
                vec![
                    Row::Quota,
                    Row::Cold,
                    Row::ColdOutlook,
                    Row::ColdMinTokens,
                    Row::ColdIdleMin,
                ],
            ),
            (
                "session",
                vec![
                    Row::SessionHeaders,
                    Row::PingHeader,
                    Row::Awake,
                    Row::TranscriptRoots,
                ],
            ),
            (
                "notices",
                std::iter::once(Row::NoticeDefault)
                    .chain((0..notices).map(Row::Notice))
                    .collect(),
            ),
            ("fixed here", vec![Row::Port, Row::Store]),
        ]
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        let rows = self.rows().len();
        if rows == 0 {
            self.selected = 0;
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, rows as isize - 1) as usize;
    }

    pub(super) fn select(&mut self, index: usize) {
        let rows = self.rows().len();
        if index < rows {
            self.selected = index;
        }
    }

    /// Adopt a reply to the configuration service. A refused edit leaves
    /// everything as it was and is reported; a successful one is followed by
    /// a fresh read, because the service is what decides what the rows now
    /// say.
    pub(super) fn receive(&mut self, reply: Reply) -> Option<Command> {
        self.in_flight = false;
        match reply {
            Reply::Command(_, Err(error)) => {
                self.unreachable = true;
                self.pending_clears.clear();
                // A form is offered only while the service is there to take
                // it (spec: UTUI#the-configuration-pages).
                self.editing = None;
                self.error = Some(error);
                None
            }
            Reply::Command(command, Ok(response)) => {
                self.unreachable = false;
                if command.name == "snapshot" {
                    return self.receive_snapshot(response);
                }
                if response.status / 100 == 2 {
                    self.error = None;
                    if self.editing.as_ref().is_some_and(|editing| editing.sent) {
                        self.editing = None;
                    }
                    if command.name == "safeguards-clear"
                        && let Some(next) = self.pending_clears.pop()
                    {
                        return Some(self.clear_command(next));
                    }
                    return Some(Command::snapshot("config-snapshot"));
                }
                self.pending_clears.clear();
                let said = refusal(&response);
                match self.editing.as_mut().filter(|editing| editing.sent) {
                    // Beside the field that held it, with the form kept as
                    // it was typed.
                    Some(editing) => {
                        editing.sent = false;
                        editing.refusal = Some(said);
                    }
                    None => self.error = Some(said),
                }
                None
            }
        }
    }

    /// Adopt a reply to the routing snapshot, which tells what depends on
    /// each provider. It never says the service is down: that is the
    /// configuration read's to say.
    pub(super) fn receive_routing(&mut self, reply: Reply) {
        let Reply::Command(_, result) = reply;
        self.dependents = match result {
            Ok(response) if response.status / 100 == 2 => parse_dependents(&response.body),
            _ => None,
        };
    }

    fn receive_snapshot(&mut self, response: Response) -> Option<Command> {
        if response.status / 100 != 2 {
            self.error = Some(refusal(&response));
            return None;
        }
        let snapshot = parse_snapshot(&response.body);
        let problem = snapshot
            .providers
            .iter()
            .position(Provider::is_not_configured);
        self.snapshot = Some(snapshot);
        if !self.opened {
            self.opened = true;
            if self.view == View::Providers {
                self.selected = problem.unwrap_or(0);
            }
        }
        let limit = self.rows().len();
        if limit > 0 && self.selected >= limit {
            self.selected = limit - 1;
        }
        self.error = None;
        None
    }

    pub(super) fn command_refused(&mut self, name: &'static str) {
        self.in_flight = false;
        self.pending_clears.clear();
        self.unreachable = true;
        self.editing = None;
        self.error = Some(format!("{name}: the service is not answering"));
    }

    /// Handle a key, returning the command it turns into.
    pub(super) fn key(&mut self, key: KeyEvent) -> Option<Command> {
        if self.editing.is_some() {
            return self.edit_key(key);
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                None
            }
            // Two views, so either way is the other one.
            KeyCode::Left | KeyCode::Right | KeyCode::Char('h' | 'l') => {
                self.set_view(match self.view {
                    View::Providers => View::Settings,
                    View::Settings => View::Providers,
                });
                None
            }
            KeyCode::Char('1') => {
                self.set_view(View::Providers);
                None
            }
            KeyCode::Char('2') => {
                self.set_view(View::Settings);
                None
            }
            KeyCode::Char(' ') => self.toggle_selected(),
            KeyCode::Enter => match self.view {
                View::Providers => {
                    self.open_route();
                    None
                }
                View::Settings => self.act(),
            },
            KeyCode::Char('c') => self.clear_marks(),
            KeyCode::Char(key @ ('e' | 'u' | 'f' | 'v')) => {
                self.begin(key);
                None
            }
            KeyCode::Char('p') => self.toggle_probe(),
            _ => None,
        }
    }

    /// The provider the selected row is, in the providers view.
    fn focused(&self) -> Option<(usize, &Provider)> {
        if self.view != View::Providers {
            return None;
        }
        match self.row()? {
            Row::Provider(index) => Some((index, self.snapshot.as_ref()?.providers.get(index)?)),
            _ => None,
        }
    }

    /// Ask to be taken to the first route that reaches the selected
    /// provider, which the loop does on the routing page.
    fn open_route(&mut self) {
        if !self.reachable() {
            return;
        }
        let Some((_, provider)) = self.focused() else {
            return;
        };
        let name = provider.name.clone();
        self.goto = self
            .dependents(&name)
            .and_then(|found| found.routes.first().cloned());
    }

    /// Clear the rejection marks held against the focused provider's
    /// bindings, one request each. Holding none is said rather than sent,
    /// since the service would answer that nothing was cleared.
    fn clear_marks(&mut self) -> Option<Command> {
        if !self.reachable() {
            return None;
        }
        let provider = self.focused()?.1.name.clone();
        let mut marked: Vec<String> = self
            .snapshot
            .as_ref()?
            .safeguards
            .iter()
            .filter(|binding| binding.provider == provider && binding.marked_ms.is_some())
            .map(|binding| binding.binding_id.clone())
            .collect();
        if marked.is_empty() {
            self.error = Some(format!("{provider}: no rejection mark is held"));
            return None;
        }
        self.error = None;
        // Popped from the end, so reversed to go in snapshot order.
        marked.reverse();
        let first = marked.pop()?;
        self.pending_clears = marked;
        Some(self.clear_command(first))
    }

    /// The request that forgets one binding's rejection mark. It lives under
    /// `/_toker/safeguards`, beside the config namespace and not in it.
    fn clear_command(&mut self, binding_id: String) -> Command {
        self.in_flight = true;
        Command {
            name: "safeguards-clear",
            method: Method::POST,
            path: "/safeguards/mark/clear",
            control: "safeguards-mark-clear",
            body: Some(json!({ "binding_id": binding_id })),
            query: None,
        }
    }

    fn edit_key(&mut self, key: KeyEvent) -> Option<Command> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let editing = self.editing.as_mut()?;
        match key.code {
            KeyCode::Esc => {
                self.editing = None;
                None
            }
            // A form with one field saves where it is, as the save control
            // says.
            KeyCode::Enter => self.save(),
            KeyCode::Char('s') if ctrl => self.save(),
            KeyCode::Backspace => {
                editing.buffer.pop();
                editing.refusal = None;
                None
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                editing.buffer.push(c);
                editing.refusal = None;
                None
            }
            _ => None,
        }
    }

    /// Act on the selected row in the settings view: toggle it, or open its
    /// form.
    fn act(&mut self) -> Option<Command> {
        let row = self.row()?;
        if !row.is_editable() || !self.reachable() {
            return None;
        }
        if row.is_toggle() {
            return self.toggle(row);
        }
        self.open(Target::Setting(row));
        None
    }

    fn toggle_selected(&mut self) -> Option<Command> {
        let row = self.row()?;
        if !row.is_editable() || !row.is_toggle() || !self.reachable() {
            return None;
        }
        self.toggle(row)
    }

    /// Open a form for the key pressed on the selected row.
    fn begin(&mut self, key: char) {
        if !self.reachable() {
            return;
        }
        let target = match (self.view, key) {
            (View::Providers, _) => {
                let Some((index, provider)) = self.focused() else {
                    return;
                };
                match key {
                    'e' if provider.holds_a_credential() => Target::Credential(index),
                    'u' => Target::Endpoint(index),
                    'f' if provider.has_handshake_settings() => Target::RefreshUrl(index),
                    'v' if provider.has_handshake_settings() => Target::ClientVersion(index),
                    _ => return,
                }
            }
            (View::Settings, 'e') => match self.row() {
                Some(row) if row.is_editable() && !row.is_toggle() => Target::Setting(row),
                _ => return,
            },
            _ => return,
        };
        self.open(target);
    }

    fn open(&mut self, target: Target) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        self.editing = Some(Editing {
            target,
            buffer: current_text(target, snapshot),
            secret: matches!(target, Target::Credential(_)),
            refusal: None,
            sent: false,
        });
    }

    fn toggle(&mut self, row: Row) -> Option<Command> {
        let snapshot = self.snapshot.as_ref()?;
        match row {
            Row::Provider(index) => {
                let provider = snapshot.providers.get(index)?;
                let mut body = provider_body(provider);
                body["enabled"] = json!(!provider.enabled);
                Some(self.put("provider", "config-edit", body))
            }
            Row::Quota | Row::Cold | Row::ColdOutlook | Row::Awake => {
                let mut settings = snapshot.settings.clone();
                let flip = |value: &mut Option<bool>, default: bool| {
                    *value = Some(!value.unwrap_or(default));
                };
                match row {
                    Row::Quota => flip(&mut settings.quota_enabled, true),
                    Row::Cold => flip(&mut settings.cold_enabled, true),
                    Row::ColdOutlook => flip(&mut settings.cold_outlook, true),
                    _ => flip(&mut settings.awake, true),
                }
                Some(self.put("settings", "config-edit", settings_body(&settings)))
            }
            _ => None,
        }
    }

    /// Switch the codex login handshake's version probe, which is on unless
    /// a version is pinned.
    fn toggle_probe(&mut self) -> Option<Command> {
        if !self.reachable() {
            return None;
        }
        let (_, provider) = self.focused()?;
        if !provider.has_handshake_settings() {
            return None;
        }
        let mut body = provider_body(provider);
        body["version_probe"] = json!(!provider.probes_version());
        Some(self.put("provider", "config-edit", body))
    }

    /// Turn the open form into its request, if what it holds is acceptable.
    /// The form stays open until the service has taken it, so a refusal can
    /// be shown beside the field with what was typed still in it.
    fn save(&mut self) -> Option<Command> {
        let snapshot = self.snapshot.as_ref()?;
        let editing = self.editing.as_ref()?;
        if editing.sent || !editing.saveable(snapshot) {
            return None;
        }
        let (change, _) = judge(editing.target, &editing.buffer, snapshot).ok()?;
        let command = self.command_for(editing.target, change)?;
        self.editing.as_mut()?.sent = true;
        Some(command)
    }

    fn command_for(&mut self, target: Target, change: Change) -> Option<Command> {
        let snapshot = self.snapshot.as_ref()?;
        match (target, change) {
            (Target::Endpoint(index), Change::Text(value)) => {
                let mut body = provider_body(snapshot.providers.get(index)?);
                body["upstream"] = json!(value);
                Some(self.put("provider", "config-edit", body))
            }
            (Target::RefreshUrl(index), Change::Text(value)) => {
                let mut body = provider_body(snapshot.providers.get(index)?);
                body["refresh_url"] = json!(value);
                Some(self.put("provider", "config-edit", body))
            }
            (Target::ClientVersion(index), Change::Text(value)) => {
                let provider = snapshot.providers.get(index)?;
                let mut body = provider_body(provider);
                body["client_version"] = json!(value);
                Some(self.put("provider", "config-edit", body))
            }
            (Target::Credential(index), Change::Text(value)) => {
                let provider = snapshot.providers.get(index)?;
                // The credential is the only value that moves through its
                // own operation, and never comes back (spec: DCTRL).
                Some(self.put(
                    "credential",
                    "config-credential",
                    json!({
                        "provider": provider.name,
                        "env": provider.cred_env,
                        "keyring": provider.keyring,
                        "literal": value,
                    }),
                ))
            }
            (Target::Setting(Row::Notice(at)), Change::Text(style)) => {
                let profile = snapshot.profiles().get(at)?.clone();
                Some(self.put(
                    "notice",
                    "config-edit",
                    json!({ "profile": profile, "style": style }),
                ))
            }
            (Target::Setting(row), change) => {
                let mut settings = snapshot.settings.clone();
                match (row, change) {
                    (Row::ColdMinTokens, Change::Tokens(tokens)) => {
                        settings.cold_min_tokens = tokens
                    }
                    (Row::ColdIdleMin, Change::Minutes(minutes)) => {
                        settings.cold_idle_min = minutes
                    }
                    (Row::SessionHeaders, Change::List(names)) => settings.session_headers = names,
                    (Row::TranscriptRoots, Change::List(paths)) => {
                        settings.transcript_roots = paths
                    }
                    (Row::PingHeader, Change::Text(name)) => settings.ping_header = name,
                    (Row::NoticeDefault, Change::Text(style)) => settings.notice_default = style,
                    _ => return None,
                }
                Some(self.put("settings", "config-edit", settings_body(&settings)))
            }
            _ => None,
        }
    }

    fn put(&mut self, path: &'static str, control: &'static str, body: Value) -> Command {
        self.in_flight = true;
        Command {
            name: "edit",
            method: Method::PUT,
            path,
            control,
            body: Some(body),
            query: None,
        }
    }
}

/// What a form opens with: the value as it is stored, so it can be changed
/// rather than retyped. A credential is never read back, so it opens empty.
fn current_text(target: Target, snapshot: &Snapshot) -> String {
    let provider = |index: usize| snapshot.providers.get(index);
    match target {
        Target::Endpoint(index) => provider(index)
            .and_then(|found| found.upstream.clone())
            .unwrap_or_default(),
        Target::RefreshUrl(index) => provider(index)
            .and_then(|found| found.refresh_url.clone())
            .unwrap_or_default(),
        Target::ClientVersion(index) => provider(index)
            .and_then(|found| found.client_version.clone())
            .unwrap_or_default(),
        Target::Credential(_) => String::new(),
        Target::Setting(Row::ColdMinTokens) => snapshot
            .settings
            .cold_min_tokens
            .map(check::tokens_shown)
            .unwrap_or_default(),
        Target::Setting(Row::ColdIdleMin) => snapshot
            .settings
            .cold_idle_min
            .map(|minutes| minutes.to_string())
            .unwrap_or_default(),
        Target::Setting(Row::SessionHeaders) => snapshot
            .settings
            .session_headers
            .as_ref()
            .map(|names| names.join(", "))
            .unwrap_or_default(),
        Target::Setting(Row::TranscriptRoots) => snapshot
            .settings
            .transcript_roots
            .as_ref()
            .map(|paths| paths.join(", "))
            .unwrap_or_default(),
        Target::Setting(Row::PingHeader) => {
            snapshot.settings.ping_header.clone().unwrap_or_default()
        }
        Target::Setting(Row::NoticeDefault) => {
            snapshot.settings.notice_default.clone().unwrap_or_default()
        }
        Target::Setting(Row::Notice(at)) => snapshot
            .profiles()
            .get(at)
            .and_then(|profile| snapshot.notices.get(profile))
            .cloned()
            .unwrap_or_default(),
        Target::Setting(_) => String::new(),
    }
}

/// A provider row as the edit operation takes it. Credentials are absent:
/// they move only through the credential operation.
fn provider_body(provider: &Provider) -> Value {
    json!({
        "provider": provider.name,
        "enabled": provider.enabled,
        "upstream": provider.upstream,
        "refresh_url": provider.refresh_url,
        "client_version": provider.client_version,
        "version_probe": provider.version_probe,
    })
}

fn settings_body(settings: &Settings) -> Value {
    json!({
        "quota_enabled": settings.quota_enabled,
        "cold_enabled": settings.cold_enabled,
        "cold_outlook": settings.cold_outlook,
        "cold_min_tokens": settings.cold_min_tokens,
        "cold_idle_min": settings.cold_idle_min,
        "awake": settings.awake,
        "ping_schedule": settings.ping_schedule,
        "ping_header": settings.ping_header,
        "notice_default": settings.notice_default,
        "session_headers": settings.session_headers,
        "transcript_roots": settings.transcript_roots,
    })
}

/// What a refusal says. The control surface names the setting it refused,
/// so the page shows that rather than a bare status.
fn refusal(response: &Response) -> String {
    let setting = response.body.get("setting").and_then(Value::as_str);
    let error = response
        .body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("refused");
    match setting {
        Some(setting) => format!("{setting}: {error}"),
        None => format!("refused ({})", response.status),
    }
}

fn parse_snapshot(body: &Value) -> Snapshot {
    let settings = body.get("settings").cloned().unwrap_or(Value::Null);
    let flag = |name: &str| settings.get(name).and_then(Value::as_bool);
    Snapshot {
        safeguards: body
            .get("safeguards")
            .and_then(Value::as_array)
            .map(|rows| rows.iter().map(parse_safeguards).collect())
            .unwrap_or_default(),
        providers: body
            .get("providers")
            .and_then(Value::as_array)
            .map(|rows| rows.iter().map(parse_provider).collect())
            .unwrap_or_default(),
        settings: Settings {
            quota_enabled: flag("quota_enabled"),
            cold_enabled: flag("cold_enabled"),
            cold_outlook: flag("cold_outlook"),
            cold_min_tokens: settings.get("cold_min_tokens").and_then(Value::as_u64),
            cold_idle_min: settings.get("cold_idle_min").and_then(Value::as_f64),
            awake: flag("awake"),
            ping_schedule: flag("ping_schedule"),
            ping_header: text_of(&settings, "ping_header"),
            notice_default: text_of(&settings, "notice_default"),
            session_headers: names(&settings, "session_headers"),
            transcript_roots: names(&settings, "transcript_roots"),
        },
        notices: body
            .get("notices")
            .and_then(Value::as_object)
            .map(|styles| {
                styles
                    .iter()
                    .filter_map(|(profile, style)| {
                        Some((profile.clone(), style.as_str()?.to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        largest_context_window: body.get("largest_context_window").and_then(Value::as_u64),
        cold_min_unreachable: body.get("cold_min_unreachable") == Some(&Value::Bool(true)),
        db_path: body
            .get("db_path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        port: body.get("port").and_then(Value::as_u64).unwrap_or_default(),
    }
}

fn parse_provider(row: &Value) -> Provider {
    let credential = row.get("credential").cloned().unwrap_or(Value::Null);
    Provider {
        name: row
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        enabled: row
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or_default(),
        upstream: text_of(row, "upstream"),
        cred_env: text_of(&credential, "env"),
        env_set: credential
            .get("env_set")
            .and_then(Value::as_bool)
            .unwrap_or_default(),
        keyring: credential
            .get("keyring")
            .and_then(Value::as_bool)
            .unwrap_or_default(),
        literal_set: credential
            .get("literal_set")
            .and_then(Value::as_bool)
            .unwrap_or_default(),
        refresh_url: text_of(row, "refresh_url"),
        client_version: text_of(row, "client_version"),
        version_probe: row.get("version_probe").and_then(Value::as_bool),
    }
}

fn parse_safeguards(row: &Value) -> Safeguards {
    let word = |name: &str| {
        row.get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Safeguards {
        binding_id: word("binding_id"),
        provider: word("provider"),
        // An absent word reads as unknown, which is what an earlier service
        // that did not report it would have been declaring.
        declared: Some(word("declared"))
            .filter(|declared| !declared.is_empty())
            .unwrap_or_else(|| "unknown".to_owned()),
        marked_ms: row.get("marked_ms").and_then(Value::as_u64),
    }
}

/// What routing depends on each provider, from the routing snapshot's
/// derived facts; `None` where the live revision has none, as one that did
/// not compile.
fn parse_dependents(body: &Value) -> Option<Vec<Dependents>> {
    let providers = body
        .get("derived")?
        .get("active")?
        .get("providers")?
        .as_array()?;
    let count = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(|group| group.get("count"))
            .and_then(Value::as_u64)
            .unwrap_or_default() as usize
    };
    Some(
        providers
            .iter()
            .map(|provider| {
                let strings = |value: Option<&Value>| -> Vec<String> {
                    value
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default()
                };
                Dependents {
                    provider: provider
                        .get("provider")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    bindings: provider
                        .get("bindings")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .map(|item| {
                                    let word = |key: &str| {
                                        item.get(key)
                                            .and_then(Value::as_str)
                                            .unwrap_or_default()
                                            .to_owned()
                                    };
                                    (word("binding_id"), word("cost_class"))
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    presentations: count(provider, "presentations"),
                    rewrites: count(provider, "rewrites"),
                    rewrites_by_protocol: provider
                        .get("rewrites")
                        .and_then(|rewrites| rewrites.get("by_protocol"))
                        .and_then(Value::as_object)
                        .map(|protocols| {
                            protocols
                                .iter()
                                .filter_map(|(name, hits)| {
                                    Some((name.clone(), hits.as_u64()? as usize))
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    defaults: strings(
                        provider
                            .get("defaults")
                            .and_then(|defaults| defaults.get("scopes")),
                    ),
                    routes: strings(provider.get("routes").and_then(|routes| routes.get("ids"))),
                }
            })
            .collect(),
    )
}

/// A list setting, kept as it was: absent and empty are different answers.
fn names(value: &Value, name: &str) -> Option<Vec<String>> {
    value.get(name).and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    })
}

fn text_of(value: &Value, name: &str) -> Option<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|text| !text.is_empty())
}
