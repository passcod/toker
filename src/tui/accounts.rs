//! The accounts page: the logins toker has learned, and the credentials
//! it has seen but cannot yet place (spec: ACCT#an-account).
//!
//! Read from the store rather than through the service, the way the
//! dashboard reads it: accounts are learned from traffic the service
//! already recorded, so there is nothing to ask it for. The one write is
//! naming an account, which is the operator's end of the discovery chain
//! (spec: ACCT#naming-an-account).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::chrome::{self, Chrome, Noun, RowHit};

use crate::store::{Account, AccountIdentity, AccountNameSource, CredentialAccount, Store};

/// One row of the page: a learned account, or a credential no source has
/// named. The two are listed together because they are the same question
/// asked twice — whose traffic is this? — and the second is the one the
/// operator can answer.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Entry {
    Known(Account),
    Unresolved(CredentialAccount),
}

impl Entry {
    fn provider(&self) -> &str {
        match self {
            Entry::Known(account) => &account.provider,
            Entry::Unresolved(credential) => &credential.provider,
        }
    }

    fn last_seen_ms(&self) -> i64 {
        match self {
            Entry::Known(account) => account.last_seen_ms,
            Entry::Unresolved(credential) => credential.last_seen_ms,
        }
    }
}

pub(super) struct Page {
    entries: Vec<Entry>,
    selected: usize,
    /// How many rows each unresolved credential already carries, by
    /// digest. Counted rather than hidden: they are held against the
    /// digest and move to the account when it is named.
    attributed: std::collections::BTreeMap<String, i64>,
    /// The address being typed, while the naming entry is open.
    naming: Option<String>,
    /// What went wrong with the last write, said rather than swallowed.
    error: Option<String>,
    /// Where the last frame put each entry's rows, for the mouse.
    rows: std::cell::RefCell<Vec<RowHit>>,
}

impl Page {
    pub(super) fn new(store: &Store) -> Page {
        let mut page = Page {
            entries: Vec::new(),
            selected: 0,
            attributed: std::collections::BTreeMap::new(),
            naming: None,
            error: None,
            rows: Default::default(),
        };
        page.refresh(store);
        page
    }

    /// Re-read the accounts. A failure leaves the last list standing and
    /// says so: an empty page would read as "no accounts", which is a
    /// different fact from "the store could not be read".
    pub(super) fn refresh(&mut self, store: &Store) {
        let accounts = match store.load_accounts() {
            Ok(accounts) => accounts,
            Err(error) => {
                self.error = Some(format!("the accounts could not be read: {error}"));
                return;
            }
        };
        let credentials = match store.load_credential_accounts() {
            Ok(credentials) => credentials,
            Err(error) => {
                self.error = Some(format!("the credentials could not be read: {error}"));
                return;
            }
        };
        self.attributed = credentials
            .iter()
            .filter(|credential| credential.account_number.is_none())
            .map(|credential| {
                let rows = store
                    .rows_for_digest(&credential.provider, credential.digest.as_str())
                    .unwrap_or(0);
                (credential.digest.as_str().to_owned(), rows)
            })
            .collect();
        let mut entries: Vec<Entry> = accounts.into_iter().map(Entry::Known).collect();
        entries.extend(
            credentials
                .into_iter()
                .filter(|credential| credential.account_number.is_none())
                .map(Entry::Unresolved),
        );
        self.entries = entries;
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        self.error = None;
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() - 1;
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }

    /// Whether the naming entry is open, taking every key.
    pub(super) fn naming(&self) -> bool {
        self.naming.is_some()
    }

    /// Handle a key. Returns true while the naming entry has it, so the
    /// loop knows the key was swallowed rather than ignored.
    pub(super) fn key(&mut self, key: KeyEvent, store: &Store) -> bool {
        let Some(typed) = self.naming.as_mut() else {
            if key.code == KeyCode::Char('n') && !self.entries.is_empty() {
                self.naming = Some(String::new());
                return true;
            }
            return false;
        };
        match key.code {
            KeyCode::Esc => self.naming = None,
            KeyCode::Backspace => {
                typed.pop();
            }
            KeyCode::Char(ch) => typed.push(ch),
            KeyCode::Enter => {
                let typed = self.naming.take().unwrap_or_default();
                self.name(store, typed.trim());
            }
            _ => {}
        }
        true
    }

    /// Record the operator's name for the selected entry. Naming a
    /// credential no source could place is what gives it an account, so
    /// the same call serves both kinds of row.
    fn name(&mut self, store: &Store, address: &str) {
        if address.is_empty() {
            return;
        }
        let Some(entry) = self.entries.get(self.selected) else {
            return;
        };
        let identity = AccountIdentity {
            upstream_id: None,
            email: Some(address.to_owned()),
            named_by: AccountNameSource::Operator,
        };
        let now = jiff::Timestamp::now().as_millisecond();
        let result = match entry {
            Entry::Unresolved(credential) => store.observe_credential(
                &credential.provider,
                &credential.digest,
                now,
                Some(&identity),
            ),
            // A known account is renamed through the credential that
            // reached it, because an account is only ever named by way
            // of a credential it served.
            Entry::Known(account) => match store.credential_of(account.number) {
                Ok(Some(digest)) => {
                    store.observe_credential(&account.provider, &digest, now, Some(&identity))
                }
                Ok(None) => {
                    self.error =
                        Some("that account has no credential on record to name it by".to_owned());
                    return;
                }
                Err(error) => Err(error),
            },
        };
        match result {
            Ok(_) => self.refresh(store),
            Err(error) => self.error = Some(format!("naming it failed: {error}")),
        }
    }

    /// Draw the page, with its key overlay over it when `overlay` is set,
    /// and return where the header's chips landed.
    pub(super) fn render(&self, frame: &mut Frame, now_ms: i64, overlay: bool) -> chrome::Header {
        // A failed read or write says so over the last list read, which
        // stays standing: an empty page would read as "no accounts".
        let (header, body) = chrome::page(frame, self.error.as_deref());
        let width = usize::from(body.width.saturating_sub(2));
        let entries: Vec<Vec<Line<'static>>> = if self.entries.is_empty() {
            vec![vec![Line::styled(
                "no account has served yet",
                Style::new().dim(),
            )]]
        } else {
            self.entries
                .iter()
                .enumerate()
                .map(|(index, entry)| self.entry_lines(entry, index == self.selected, width, now_ms))
                .collect()
        };
        let need = entries.iter().map(Vec::len).sum();
        let inner = chrome::panel(frame, body, "ACCOUNTS", Chrome::fitting(need, body.height));
        let selected = (!self.entries.is_empty()).then_some(self.selected);
        let rows = chrome::list(frame, inner, entries, selected, NOUN);
        self.rows.replace(if selected.is_some() { rows } else { Vec::new() });
        chrome::chips(frame, &header);
        if overlay {
            chrome::overlay(
                frame,
                "KEYS",
                &[chrome::Section {
                    heading: None,
                    rows: &KEYS,
                }],
            );
        }
        header
    }

    /// Select the entry whose row holds `at`, if any.
    pub(super) fn click(&mut self, at: ratatui::layout::Position) {
        if self.naming.is_some() {
            return;
        }
        if let Some(entry) = chrome::entry_at(&self.rows.borrow(), at) {
            self.selected = entry;
        }
    }

    fn entry_lines(
        &self,
        entry: &Entry,
        selected: bool,
        width: usize,
        now_ms: i64,
    ) -> Vec<Line<'static>> {
        use unicode_width::UnicodeWidthStr;

        // The number, then what the account is called. A credential that
        // resolved to no account carries no number — a number is assigned
        // when an account becomes known — so it sits under a dim `?`.
        let (mark, mark_style, name, name_style) = match entry {
            Entry::Known(account) => (
                format!("{} ", account.number),
                Style::new().fg(Color::Cyan),
                account
                    .email
                    .clone()
                    .or_else(|| account.upstream_id.clone())
                    .unwrap_or_else(|| "unnamed".to_owned()),
                Style::new().fg(Color::Cyan),
            ),
            Entry::Unresolved(credential) => (
                "? ".to_owned(),
                Style::new().dim(),
                format!("{}…", &credential.digest.as_str()[..8]),
                Style::new().fg(Color::Cyan),
            ),
        };
        let seen = format!("last seen {}", ago(now_ms - entry.last_seen_ms()));
        let head_used = mark.width() + 2 + name.width() + seen.width();
        let head = vec![
            Span::styled(mark, mark_style),
            Span::raw("  "),
            Span::styled(name, name_style),
            Span::raw(" ".repeat(width.saturating_sub(head_used))),
            Span::styled(seen, Style::new().dim()),
        ];

        let field = |label: &str, value: Vec<Span<'static>>| -> Line<'static> {
            let mut spans = vec![Span::raw(format!("    {label:<19}"))];
            spans.extend(value);
            Line::from(spans)
        };
        let mut lines = vec![Line::from(head)];
        lines.push(field(
            "provider",
            vec![Span::raw(entry.provider().to_owned())],
        ));
        match entry {
            Entry::Known(account) => lines.push(field(
                "identity",
                vec![Span::raw(
                    match account.named_by {
                        AccountNameSource::Local => "from another program's credential store",
                        AccountNameSource::Upstream => "from the upstream",
                        AccountNameSource::Operator => "you named it",
                    }
                    .to_owned(),
                )],
            )),
            Entry::Unresolved(credential) => {
                lines.push(field(
                    "identity",
                    vec![
                        Span::styled("not resolved", Style::new().fg(Color::Yellow)),
                        Span::styled(" · no source has named this credential", Style::new().dim()),
                    ],
                ));
                // Counted rather than hidden: they are held against the
                // digest and move to the account when it is named.
                let rows = self
                    .attributed
                    .get(credential.digest.as_str())
                    .copied()
                    .unwrap_or(0);
                lines.push(field(
                    "attributed",
                    vec![
                        Span::raw(format!("{rows} requests")),
                        Span::styled(" · backfilled when it is named", Style::new().dim()),
                    ],
                ));
            }
        }
        // The entry takes every key, `?` included, so it says them itself
        // rather than leaving them to the overlay.
        if selected && let Some(typed) = &self.naming {
            lines.push(field(
                "name it",
                vec![
                    Span::raw(typed.clone()),
                    Span::styled("▏", Style::new().fg(Color::Cyan)),
                ],
            ));
            lines.push(field(
                "",
                vec![Span::styled(
                    chrome::hints(&[("enter", "save"), ("esc", "cancel")]),
                    Style::new().dim(),
                )],
            ));
        }
        lines
    }
}

/// What the counts call the entries they leave out.
const NOUN: Noun = Noun {
    one: "account",
    many: "accounts",
};

/// The page's keys, in its `?` overlay.
const KEYS: [(&str, &str); 6] = [
    ("↑/↓ j/k", "select"),
    ("n", "name the selected account"),
    ("r", "refresh"),
    ("esc", "back to the dashboard"),
    ("tab or ⇄", "next page"),
    ("q", "quit"),
];

/// A rough age, the way every other elapsed figure on the dashboard
/// reads: the largest unit that still carries a whole number.
fn ago(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    match secs {
        ..60 => format!("{secs}s ago"),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86_400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    const NOW: i64 = 1_767_000_000_000;

    fn store() -> Store {
        Store::open(":memory:").expect("open the in-memory store")
    }

    fn observe(store: &Store, token: &str, email: Option<&str>, at_ms: i64) {
        let digest = crate::providers::credential_digest(token);
        let identity = email.map(|email| AccountIdentity {
            upstream_id: None,
            email: Some(email.to_owned()),
            named_by: AccountNameSource::Local,
        });
        store
            .observe_credential("anthropic_sub", &digest, at_ms, identity.as_ref())
            .expect("observe the credential");
    }

    fn rendered(page: &Page, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| {
                page.render(frame, NOW, false);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn the_page_lists_accounts_and_the_credentials_no_source_has_named() {
        let store = store();
        observe(&store, "ana-token", Some("ana@example.net"), NOW - 120_000);
        observe(
            &store,
            "ben-token",
            Some("ben@example.net"),
            NOW - 4 * 86_400_000,
        );
        observe(&store, "mystery-token", None, NOW - 18_000);
        let page = Page::new(&store);

        let text = rendered(&page, 100, 24);
        assert!(text.contains("ACCOUNTS"), "{text}");
        assert!(text.contains("1 "), "{text}");
        assert!(text.contains("ana@example.net"), "{text}");
        assert!(text.contains("ben@example.net"), "{text}");
        assert!(text.contains("last seen 2m ago"), "{text}");
        assert!(text.contains("last seen 4d ago"), "{text}");
        // The unresolved credential carries no number, because a number is
        // assigned when an account becomes known.
        assert!(text.contains("not resolved"), "{text}");
        assert!(text.contains("?  "), "{text}");
        assert!(!text.contains("n names"), "no page carries a footer: {text}");
    }

    /// The page's keys are in its overlay rather than a footer
    /// (spec: UTUI#finding-the-keys).
    #[test]
    fn the_overlay_names_the_page_keys() {
        let store = store();
        observe(&store, "ana-token", Some("ana@example.net"), NOW - 120_000);
        let page = Page::new(&store);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal
            .draw(|frame| {
                page.render(frame, NOW, true);
            })
            .expect("draw");
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("KEYS"), "{text}");
        assert!(text.contains("name the selected account"), "{text}");
        assert!(text.contains("? or esc close"), "{text}");
    }

    /// Too short for every entry in full, the others shed to their head
    /// line while the selected one keeps its fields, and a click lands on
    /// the entry drawn where it falls (spec: UTUI#what-is-shed-first).
    #[test]
    fn a_short_page_sheds_the_other_entries_to_their_head_line() {
        let store = store();
        for (index, email) in ["ana", "ben", "cal", "dee", "eve"].iter().enumerate() {
            observe(
                &store,
                &format!("{email}-token"),
                Some(&format!("{email}@example.net")),
                NOW - (index as i64 + 1) * 60_000,
            );
        }
        let mut page = Page::new(&store);
        page.move_selection(4);
        let text = rendered(&page, 80, 7);
        assert!(text.contains("ACCOUNTS ─"), "ruled, not boxed: {text}");
        assert_eq!(
            text.matches("provider").count(),
            1,
            "only the selected entry keeps its fields: {text}"
        );
        assert!(text.contains("above"), "the cut is counted: {text}");

        // The first row drawn is not the first entry once the list has
        // scrolled to the selection, so a click there names what it shows.
        let first = page.rows.borrow().first().cloned().expect("a row was drawn");
        assert!(first.entry > 0, "{:?}", page.rows.borrow());
        page.click(first.area.as_position());
        assert_eq!(page.selected, first.entry);
    }

    #[test]
    fn naming_an_unresolved_credential_gives_it_an_account() {
        let store = store();
        observe(&store, "mystery-token", None, NOW - 18_000);
        let mut page = Page::new(&store);
        assert_eq!(
            store.load_accounts().expect("accounts").len(),
            0,
            "a credential no source has named reaches no account"
        );
        assert!(matches!(page.entries[0], Entry::Unresolved(_)));

        let key =
            |code| crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        assert!(page.key(key(KeyCode::Char('n')), &store), "the entry opens");
        for ch in "cal@example.net".chars() {
            assert!(page.key(key(KeyCode::Char(ch)), &store));
        }
        // Every key reaches the entry while it is open, `q` included.
        assert!(page.key(key(KeyCode::Enter), &store));

        let accounts = store.load_accounts().expect("accounts");
        assert_eq!(accounts.len(), 1, "naming it is what makes the account");
        assert_eq!(accounts[0].email.as_deref(), Some("cal@example.net"));
        assert_eq!(accounts[0].named_by, AccountNameSource::Operator);
        assert!(
            matches!(page.entries[0], Entry::Known(_)),
            "the page re-reads what it just wrote"
        );
    }

    #[test]
    fn a_page_with_nothing_on_it_says_so_rather_than_rendering_empty() {
        let page = Page::new(&store());
        let text = rendered(&page, 60, 10);
        assert!(text.contains("no account has served yet"), "{text}");
    }
}
