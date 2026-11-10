//! Routing policy page.

mod data;
mod edit_view;
mod editing;
mod form;
#[cfg(test)]
mod frames_tests;
mod history;
#[cfg(test)]
mod history_tests;
mod keys;
mod model;
mod models_tab;
mod reconcile;
mod routes;
mod rules;
mod status;
mod table;
mod template;
#[cfg(test)]
mod testing;
mod trace;
#[cfg(test)]
mod trace_tests;
mod view;
mod words;

use crate::tui::control::{Client, Command, Reply};
use crossterm::event::KeyEvent;
use model::State;
use reconcile::Reconciler;
pub use reconcile::SettingsSync;

/// The revision history read: independent data the page loads eagerly.
fn revisions() -> Command {
    Command {
        name: "revisions",
        method: reqwest::Method::GET,
        path: "revisions",
        control: "routing-revisions",
        body: None,
    }
}

/// Whether `reply` was an activation that landed, which is what makes the
/// page reconcile a client's settings afterwards.
fn activated(reply: &Reply) -> bool {
    match reply {
        Reply::Command(command, Ok(response)) => {
            matches!(command.name, "save" | "reactivate") && response.status / 100 == 2
        }
        Reply::Command(_, Err(_)) => false,
    }
}

/// The keys the routing page acts on itself, outside a modal. The dashboard
/// loop forwards exactly these, so a key the page handles but this list lacks
/// is silently dead.
pub(super) fn owns_key(code: crossterm::event::KeyCode) -> bool {
    matches!(
        code,
        crossterm::event::KeyCode::Char('e' | 'n' | 'd' | 'm' | 'a' | 'p' | 't' | 'c')
            | crossterm::event::KeyCode::Enter
            | crossterm::event::KeyCode::PageUp
            | crossterm::event::KeyCode::PageDown
    )
}

/// Page-owned state, including the bounded control worker.
pub(super) struct Page {
    client: Client,
    reconciler: Reconciler,
    state: State,
    /// Where the last frame put things, for the mouse.
    drawn: std::cell::RefCell<view::Drawn>,
}

impl Page {
    pub(super) fn new(port: u16, settings: Option<SettingsSync>) -> Page {
        let client = Client::new(port, "routing", "routing-snapshot");
        let mut page = Page {
            client,
            reconciler: Reconciler::new(settings),
            state: State::default(),
            drawn: Default::default(),
        };
        page.refresh();
        // Revisions are independent data: load them eagerly, but never make an
        // unavailable new endpoint stop the established read-only snapshot.
        page.send(revisions());
        page
    }

    pub(super) fn refresh(&mut self) {
        if self.state.begin_refresh() && !self.client.refresh() {
            self.state.refresh_refused();
        }
    }

    pub(super) fn poll(&mut self) {
        while let Some(reply) = self.client.poll() {
            let activated = activated(&reply);
            self.state.receive(reply);
            self.check();
            if activated {
                // The reply carries the activation; the derived facts and the
                // history behind it are read again.
                self.refresh();
                self.send(revisions());
                if let Some(note) = self.reconciler.start() {
                    self.state.set_settings_note(note);
                }
            }
        }
        while let Some(note) = self.reconciler.poll() {
            self.state.set_settings_note(note);
        }
    }

    pub(super) fn move_view(&mut self, delta: isize) {
        self.state.move_view(delta);
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        self.state.move_selection(delta);
    }

    pub(super) fn set_view(&mut self, view: usize) {
        self.state.set_view(view);
    }

    /// Whether `esc` has something of the page's own to close before it
    /// leaves the page: a route opened whole.
    pub(super) fn captures_esc(&self) -> bool {
        self.state.mode() == model::Mode::Route
            || self.state.trace().is_some()
            || self.state.armed().is_some()
    }

    /// Handle a key owned by the routing page.  Only this page owns its form
    /// state, so closing one never alters dashboard dialogs or controls.
    pub(super) fn key(&mut self, key: KeyEvent) -> bool {
        let active = self.state.capturing();
        if let Some(command) = self.state.key(key) {
            self.send(command);
        }
        self.check();
        active
    }

    /// Whether a form owns every key.
    pub(super) fn modal_active(&self) -> bool {
        self.state.capturing()
    }

    /// Ask the daemon what an edit in progress needs checking, and read the
    /// snapshot again where a save found it out of date.
    fn check(&mut self) {
        if let Some(command) = self.state.next_check() {
            self.send(command);
        }
        if self.state.take_refresh_wanted() {
            self.refresh();
        }
    }

    fn send(&mut self, command: Command) {
        let name = command.name;
        if !self.client.send(command) {
            // The state deliberately retains its last snapshot and remains
            // usable for read-only navigation when a daemon is unavailable.
            self.state.command_refused(name);
        }
    }

    /// Draw the page, with its key overlay over it when `overlay` is set,
    /// and return where the header's chips landed.
    pub(super) fn render(
        &self,
        frame: &mut ratatui::Frame,
        overlay: bool,
    ) -> crate::tui::chrome::Header {
        let drawn = view::render(frame, &self.state, overlay);
        let header = drawn.header;
        self.drawn.replace(drawn);
        header
    }

    /// A left click in the body: focus the form field under it, or select
    /// the entry. Both header buttons are the loop's.
    pub(super) fn click(&mut self, at: ratatui::layout::Position) {
        let drawn = self.drawn.borrow().clone();
        if let Some(hit) = &drawn.form {
            if let Some((_, row)) = hit.fields.iter().find(|(area, _)| area.contains(at)) {
                self.state.focus_form_row(*row);
            }
            return;
        }
        if self.state.capturing() {
            return;
        }
        if let Some(tab) = drawn.tabs.iter().position(|tab| tab.contains(at)) {
            self.set_view(tab);
        } else if let Some(entry) = crate::tui::chrome::entry_at(&drawn.rows, at) {
            self.state.select(entry);
        }
    }

    /// The wheel moves what is under the pointer: the form's focus, the
    /// list's selection, or the detail pane.
    pub(super) fn wheel(&mut self, at: ratatui::layout::Position, delta: i32) {
        let drawn = self.drawn.borrow().clone();
        if drawn.form.is_some() {
            self.state.step_form(delta as isize);
        } else if self.state.capturing() {
        } else if drawn.entries.contains(at) {
            self.state.move_selection(delta as isize);
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::{activated, owns_key};
    use crate::tui::control::{Command, Reply, Response};

    fn reply(name: &'static str, status: u16) -> Reply {
        let mut command = Command::snapshot("routing-snapshot");
        command.name = name;
        Reply::Command(
            command,
            Ok(Response {
                status,
                body: serde_json::Value::Null,
            }),
        )
    }

    #[test]
    fn only_a_successful_activation_triggers_the_settings_sync() {
        assert!(activated(&reply("save", 200)));
        assert!(activated(&reply("reactivate", 200)));
        assert!(!activated(&reply("save", 409)));
        assert!(!activated(&reply("preview", 200)));
        assert!(!activated(&Reply::Command(
            Command::snapshot("routing-snapshot"),
            Err("down".to_owned())
        )));
    }

    #[test]
    fn every_key_the_legend_documents_reaches_the_page() {
        for key in ['e', 'n', 'd', 'm', 'a', 'p', 't', 'c'] {
            assert!(owns_key(KeyCode::Char(key)), "{key}");
        }
        for key in [KeyCode::PageUp, KeyCode::PageDown] {
            assert!(owns_key(key), "{key:?}");
        }
        assert!(!owns_key(KeyCode::Char('q')), "q still quits");
    }
}
