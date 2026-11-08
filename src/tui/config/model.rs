//! The configuration pages' state: what the snapshot says, what is
//! selected, and the command an edit turns into.
//!
//! The service is the only writer, so every change here is a request and
//! nothing is applied locally: the next snapshot is what the page believes
//! (spec: DCONF#editing). A refused value leaves the row as it was and is
//! reported, which falls out of never writing ahead of the reply.

use crossterm::event::{KeyCode, KeyEvent};
use reqwest::Method;
use serde_json::{Value, json};

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
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Settings {
    pub(super) quota_enabled: Option<bool>,
    pub(super) cold_enabled: Option<bool>,
    pub(super) cold_outlook: Option<bool>,
    pub(super) cold_min_tokens: Option<u64>,
    pub(super) cold_idle_min: Option<f64>,
    pub(super) awake: Option<bool>,
    pub(super) ping_header: Option<String>,
    pub(super) notice_default: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Snapshot {
    pub(super) providers: Vec<Provider>,
    pub(super) settings: Settings,
    pub(super) db_path: String,
    pub(super) port: u64,
}

/// One row of the page: what it names and what acting on it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Row {
    /// A provider's name line; acting on it switches the provider on or off.
    Provider(usize),
    Upstream(usize),
    Credential(usize),
    Quota,
    Cold,
    ColdOutlook,
    ColdMinTokens,
    ColdIdleMin,
    Awake,
    PingHeader,
    NoticeDefault,
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

/// A text entry in progress: which row it will change, and what has been
/// typed. A credential entry is never echoed back by the page.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Editing {
    pub(super) row: Row,
    pub(super) buffer: String,
    pub(super) secret: bool,
}

#[derive(Debug, Default)]
pub(super) struct State {
    view: View,
    snapshot: Option<Snapshot>,
    selected: usize,
    error: Option<String>,
    in_flight: bool,
    /// The service did not answer the last read. The pages then report that
    /// rather than offer values to edit (spec: UTUI#the-configuration-pages).
    unreachable: bool,
    editing: Option<Editing>,
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

    /// The rows the current view lists, in order.
    pub(super) fn rows(&self) -> Vec<Row> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        match self.view {
            View::Providers => (0..snapshot.providers.len())
                .flat_map(|index| {
                    let mut rows = vec![Row::Provider(index), Row::Upstream(index)];
                    if snapshot.providers[index].holds_a_credential() {
                        rows.push(Row::Credential(index));
                    }
                    rows
                })
                .collect(),
            View::Settings => vec![
                Row::Quota,
                Row::Cold,
                Row::ColdOutlook,
                Row::ColdMinTokens,
                Row::ColdIdleMin,
                Row::Awake,
                Row::PingHeader,
                Row::NoticeDefault,
                Row::Port,
                Row::Store,
            ],
        }
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

    /// Adopt a reply. A refused edit leaves everything as it was and is
    /// reported; a successful one is followed by a fresh read, because the
    /// service is what decides what the rows now say.
    pub(super) fn receive(&mut self, reply: Reply) -> Option<Command> {
        self.in_flight = false;
        match reply {
            Reply::Command(_, Err(error)) => {
                self.unreachable = true;
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
                    return Some(Command::snapshot("config-snapshot"));
                }
                self.error = Some(refusal(&response));
                None
            }
        }
    }

    fn receive_snapshot(&mut self, response: Response) -> Option<Command> {
        if response.status / 100 != 2 {
            self.error = Some(refusal(&response));
            return None;
        }
        let snapshot = parse_snapshot(&response.body);
        let rows = snapshot.providers.len();
        self.snapshot = Some(snapshot);
        if self.view == View::Providers && rows == 0 {
            self.selected = 0;
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
        self.unreachable = true;
        self.error = Some(format!("{name}: the service is not answering"));
    }

    /// Handle a key, returning the command it turns into.
    pub(super) fn key(&mut self, key: KeyEvent) -> Option<Command> {
        if self.editing.is_some() {
            return self.edit_key(key);
        }
        match key.code {
            KeyCode::Up => {
                self.move_selection(-1);
                None
            }
            KeyCode::Down => {
                self.move_selection(1);
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
            KeyCode::Char(' ') | KeyCode::Enter => self.act(),
            KeyCode::Char('e') => {
                self.begin_edit();
                None
            }
            _ => None,
        }
    }

    fn edit_key(&mut self, key: KeyEvent) -> Option<Command> {
        let editing = self.editing.as_mut()?;
        match key.code {
            KeyCode::Esc => {
                self.editing = None;
                None
            }
            KeyCode::Backspace => {
                editing.buffer.pop();
                None
            }
            KeyCode::Char(c) => {
                editing.buffer.push(c);
                None
            }
            KeyCode::Enter => {
                let editing = self.editing.take()?;
                self.submit_edit(editing)
            }
            _ => None,
        }
    }

    /// Act on the selected row: toggle it, or open an editor for it.
    fn act(&mut self) -> Option<Command> {
        let row = *self.rows().get(self.selected)?;
        if !row.is_editable() || !self.reachable() {
            return None;
        }
        if row.is_toggle() {
            return self.toggle(row);
        }
        self.begin_edit();
        None
    }

    fn begin_edit(&mut self) {
        let Some(row) = self.rows().get(self.selected).copied() else {
            return;
        };
        if !row.is_editable() || row.is_toggle() || !self.reachable() {
            return;
        }
        self.editing = Some(Editing {
            row,
            buffer: String::new(),
            secret: matches!(row, Row::Credential(_)),
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

    /// Turn a finished text entry into its request. An empty entry clears
    /// the value, which returns it to the built-in default.
    fn submit_edit(&mut self, editing: Editing) -> Option<Command> {
        let snapshot = self.snapshot.as_ref()?;
        let typed = editing.buffer.trim();
        let value = (!typed.is_empty()).then(|| typed.to_owned());
        match editing.row {
            Row::Upstream(index) => {
                let provider = snapshot.providers.get(index)?;
                let mut body = provider_body(provider);
                body["upstream"] = json!(value);
                Some(self.put("provider", "config-edit", body))
            }
            Row::Credential(index) => {
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
            Row::PingHeader | Row::NoticeDefault | Row::ColdMinTokens | Row::ColdIdleMin => {
                let mut settings = snapshot.settings.clone();
                match editing.row {
                    Row::PingHeader => settings.ping_header = value,
                    Row::NoticeDefault => settings.notice_default = value,
                    Row::ColdMinTokens => {
                        settings.cold_min_tokens = match value {
                            None => None,
                            Some(text) => match text.parse() {
                                Ok(count) => Some(count),
                                Err(_) => {
                                    self.error =
                                        Some(format!("cold min tokens: {text:?} is not a number"));
                                    return None;
                                }
                            },
                        }
                    }
                    _ => {
                        settings.cold_idle_min = match value {
                            None => None,
                            Some(text) => match text.parse() {
                                Ok(minutes) => Some(minutes),
                                Err(_) => {
                                    self.error =
                                        Some(format!("cold idle: {text:?} is not a number"));
                                    return None;
                                }
                            },
                        }
                    }
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
        }
    }
}

/// A provider row as the edit operation takes it. Credentials are absent:
/// they move only through the credential operation.
fn provider_body(provider: &Provider) -> Value {
    json!({
        "provider": provider.name,
        "enabled": provider.enabled,
        "upstream": provider.upstream,
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
        "ping_header": settings.ping_header,
        "notice_default": settings.notice_default,
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
            ping_header: text(&settings, "ping_header"),
            notice_default: text(&settings, "notice_default"),
        },
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
        upstream: text(row, "upstream"),
        cred_env: text(&credential, "env"),
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
        client_version: text(row, "client_version"),
        version_probe: row.get("version_probe").and_then(Value::as_bool),
    }
}

fn text(value: &Value, name: &str) -> Option<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn snapshot_body() -> Value {
        json!({
            "providers": [
                {
                    "provider": "anthropic_sub",
                    "enabled": true,
                    "upstream": null,
                    "credential": {
                        "env": "TOKER_ANTHROPIC_SUB_TOKEN",
                        "env_set": false,
                        "keyring": true,
                        "literal_set": false
                    }
                },
                {
                    "provider": "codex_sub",
                    "enabled": false,
                    "upstream": "http://gateway.invalid",
                    "credential": {"env": null, "env_set": false, "keyring": false,
                                   "literal_set": false}
                }
            ],
            "settings": {
                "quota_enabled": null,
                "cold_enabled": false,
                "cold_min_tokens": 175000,
                "awake": null,
                "ping_header": null,
                "notice_default": null
            },
            "db_path": "/home/example/.local/share/toker/toker.db",
            "port": 18123
        })
    }

    fn loaded() -> State {
        let mut state = State::default();
        let reply = Reply::Command(
            Command::snapshot("config-snapshot"),
            Ok(Response {
                status: 200,
                body: snapshot_body(),
            }),
        );
        assert!(state.receive(reply).is_none());
        state
    }

    #[test]
    fn the_snapshot_becomes_the_rows_the_page_lists() {
        let state = loaded();
        assert!(state.reachable());
        // Codex signs with the login it shares with the codex CLI, so it
        // has no credential row of its own.
        assert_eq!(
            state.rows(),
            vec![
                Row::Provider(0),
                Row::Upstream(0),
                Row::Credential(0),
                Row::Provider(1),
                Row::Upstream(1),
            ]
        );
    }

    #[test]
    fn a_provider_with_no_credential_reads_as_not_configured() {
        let state = loaded();
        let providers = &state.snapshot().expect("loaded").providers;
        assert!(providers[0].has_credential(), "the keyring is a source");
        assert!(!providers[1].has_credential());
        assert!(!providers[1].holds_a_credential(), "codex signs its own");
    }

    #[test]
    fn toggling_a_provider_sends_its_whole_row_without_a_credential() {
        let mut state = loaded();
        let command = state.key(key(KeyCode::Char(' '))).expect("a request");
        assert_eq!(command.path, "provider");
        assert_eq!(command.control, "config-edit");
        let body = command.body.expect("a body");
        assert_eq!(body["provider"], json!("anthropic_sub"));
        assert_eq!(body["enabled"], json!(false), "the toggle flips it");
        // The credential moves only through its own operation, so the edit
        // path never carries one in either direction (spec: DCTRL).
        assert!(body.get("cred_literal").is_none());
        assert!(body.get("credential").is_none());
    }

    #[test]
    fn nothing_is_applied_locally_until_the_service_says_so() {
        let mut state = loaded();
        state.key(key(KeyCode::Char(' ')));
        assert!(
            state.snapshot().expect("loaded").providers[0].enabled,
            "the page still shows what the service last said"
        );

        // A successful edit is followed by a fresh read, which is what the
        // page then believes.
        let next = state
            .receive(Reply::Command(
                Command {
                    name: "edit",
                    method: Method::PUT,
                    path: "provider",
                    control: "config-edit",
                    body: None,
                },
                Ok(Response {
                    status: 200,
                    body: Value::Null,
                }),
            ))
            .expect("a refresh follows");
        assert_eq!(next.name, "snapshot");
    }

    #[test]
    fn a_refusal_names_the_setting_and_changes_nothing() {
        let mut state = loaded();
        let before = state.snapshot().cloned();
        let next = state.receive(Reply::Command(
            Command {
                name: "edit",
                method: Method::PUT,
                path: "provider",
                control: "config-edit",
                body: None,
            },
            Ok(Response {
                status: 400,
                body: json!({"setting": "provider", "error": "not a url at all"}),
            }),
        ));
        assert!(next.is_none(), "a refusal is not followed by a read");
        assert_eq!(state.error(), Some("provider: not a url at all"));
        assert_eq!(state.snapshot().cloned(), before);
    }

    #[test]
    fn the_pages_report_an_unanswering_service_rather_than_offering_edits() {
        let mut state = loaded();
        assert!(state.reachable());
        state.receive(Reply::Command(
            Command::snapshot("config-snapshot"),
            Err("connection refused".to_owned()),
        ));
        assert!(!state.reachable());
        assert_eq!(state.error(), Some("connection refused"));
        // And nothing can be acted on while it is down: an edit would go
        // nowhere (spec: UTUI#the-configuration-pages).
        assert!(state.key(key(KeyCode::Char(' '))).is_none());
        assert!(state.key(key(KeyCode::Char('e'))).is_none());
        assert!(state.editing().is_none());
    }

    #[test]
    fn editing_an_endpoint_sends_what_was_typed_and_an_empty_entry_clears_it() {
        let mut state = loaded();
        state.move_selection(1);
        assert!(state.key(key(KeyCode::Char('e'))).is_none());
        for c in "http://mine.invalid".chars() {
            state.key(key(KeyCode::Char(c)));
        }
        let command = state.key(key(KeyCode::Enter)).expect("a request");
        assert_eq!(
            command.body.expect("a body")["upstream"],
            json!("http://mine.invalid")
        );

        // An empty entry clears the value, returning it to the built-in one.
        state.key(key(KeyCode::Char('e')));
        let command = state.key(key(KeyCode::Enter)).expect("a request");
        assert_eq!(command.body.expect("a body")["upstream"], Value::Null);
    }

    #[test]
    fn a_credential_goes_through_its_own_operation_and_is_never_echoed() {
        let mut state = loaded();
        state.move_selection(2);
        state.key(key(KeyCode::Char('e')));
        let editing = state.editing().expect("an entry");
        assert!(editing.secret, "a credential entry is masked");
        for c in "sk-secret".chars() {
            state.key(key(KeyCode::Char(c)));
        }
        let command = state.key(key(KeyCode::Enter)).expect("a request");
        assert_eq!(command.path, "credential");
        assert_eq!(command.control, "config-credential");
        assert_eq!(command.body.expect("a body")["literal"], json!("sk-secret"));
    }

    #[test]
    fn a_setting_that_is_not_a_number_is_refused_before_it_is_sent() {
        let mut state = loaded();
        state.set_view(View::Settings);
        state.move_selection(3);
        assert_eq!(state.rows()[3], Row::ColdMinTokens);
        state.key(key(KeyCode::Char('e')));
        for c in "lots".chars() {
            state.key(key(KeyCode::Char(c)));
        }
        assert!(
            state.key(key(KeyCode::Enter)).is_none(),
            "nothing is sent for a value that is not a number"
        );
        assert_eq!(
            state.error(),
            Some(r#"cold min tokens: "lots" is not a number"#)
        );
    }

    #[test]
    fn the_port_and_the_store_are_shown_but_not_offered() {
        let mut state = loaded();
        state.set_view(View::Settings);
        let rows = state.rows();
        for row in [Row::Port, Row::Store] {
            assert!(rows.contains(&row));
            assert!(!row.is_editable(), "{row:?} is settled before the store");
        }
        let port = rows
            .iter()
            .position(|row| *row == Row::Port)
            .expect("a row");
        state.select(port);
        assert!(state.key(key(KeyCode::Char(' '))).is_none());
        assert!(state.key(key(KeyCode::Char('e'))).is_none());
        assert!(state.editing().is_none());
    }

    #[test]
    fn an_unset_setting_toggles_off_its_default_rather_than_to_it() {
        let mut state = loaded();
        state.set_view(View::Settings);
        // quota_enabled is unset, so it follows the built-in default (on);
        // toggling it must turn it off, not set it to what it already is.
        let command = state.key(key(KeyCode::Char(' '))).expect("a request");
        assert_eq!(command.body.expect("a body")["quota_enabled"], json!(false));
    }

    #[test]
    fn switching_views_puts_the_selection_back_at_the_top() {
        let mut state = loaded();
        state.move_selection(3);
        assert_eq!(state.selected(), 3);
        state.key(key(KeyCode::Char('2')));
        assert_eq!(state.view(), View::Settings);
        assert_eq!(state.selected(), 0);
    }
}
