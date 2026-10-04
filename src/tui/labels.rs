//! Session labels: what a session is about, from Claude Code's own
//! transcript of it.
//!
//! The ledger never records content (invariant 1), so it can say how big
//! a session is but not which one it is. Claude Code already keeps a
//! transcript per session under its config directory, named by the same
//! id the proxy logs from the session header, and appends the working
//! directory, a generated title, the deliberate agent name, and the last
//! prompt as it goes. The dashboard reads that to put a name on a row;
//! this module only ever reads, and nothing it returns is written
//! anywhere — the predecessor's transcript reader, ported whole:
//!
//! - [`transcript_roots`]: every config directory to look in — Claude
//!   Code's own default, whatever `CLAUDE_CONFIG_DIR` says, and any more
//!   listed in the config's `transcript_roots`
//!   (a harness that runs its agents under a config directory of its own
//!   — Workhorse does — keeps their transcripts somewhere this shell's
//!   environment cannot see).
//! - [`find_transcript`]: the `projects/<dir>/<session-id>.jsonl`
//!   layout, with the id refused outright unless it is nothing but
//!   `[0-9a-zA-Z-]` (it is interpolated into a path).
//! - [`session_label`]: the tail-only read. A transcript is mostly tool
//!   results and a single one can run to hundreds of KiB, so only the
//!   last [`TAIL_BYTES`] are ever read, and only the few small records
//!   wanted are parsed out of them, newest first.
//! - [`Labels`]: the per-refresh cache — one read per session per
//!   refresh, never per render (the reference re-resolved on every
//!   2-second render; the tick is that render here).
//!
//! Failure is always "no label": a missing transcript, an unreadable
//! one, a tail with nothing usable — the view falls back to the session
//! id, never a panic (invariant 6's spirit).

use std::collections::HashMap;
use std::env;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// How much of a transcript is ever read
/// (`TAIL_BYTES`, the reference's tail cap). Claude Code re-appends the
/// title and last prompt every
/// turn, so they sit within a few KiB of the end — but a long turn's
/// tool results land after them, and a single one can run to hundreds of
/// KiB.
pub(crate) const TAIL_BYTES: u64 = 1024 * 1024;

/// The name, working directory, and last prompt a transcript's tail
/// carries, or [`None`] via the `Option` that holds it when it carries
/// none of them (the tail-read's output). The
/// title is the deliberate agent name when the tail carries one, else
/// the generated ai-title — the agent name wins wherever in the tail it
/// sits (`clean(name) ?? clean(title)`).
///
/// Nothing here is ever written anywhere: labels exist at view time
/// only (invariant 1), which is also why this type has no serialisation
/// — it never crosses a wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Label {
    /// The newest `cwd` the tail carries.
    pub cwd: Option<String>,
    /// The newest agent name, else the newest generated title.
    pub title: Option<String>,
    /// The newest last-prompt record's text.
    pub prompt: Option<String>,
}

/// Every transcript root to look in, each carrying the `projects/`
/// level Claude Code keeps its transcripts under: Claude Code's own
/// default (`~/.claude`), whatever this shell's `CLAUDE_CONFIG_DIR`
/// says, and any more listed in the config's `transcript_roots` — the
/// env read hoisted
/// out so the pure core ([`roots_from`]) stays testable without
/// touching process env.
pub(crate) fn transcript_roots(extra: &[PathBuf]) -> Vec<PathBuf> {
    let home = env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    let config_dir = env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from);
    roots_from(home, config_dir, extra)
}

/// The roots computation under explicit inputs: the two defaults, the
/// configured extras, `~` expanded against the home, `projects/`
/// appended, empties dropped, duplicates collapsed in order —
/// the pure mirror of the env-reading wrapper, with the configured
/// extras standing in for the colon list.
fn roots_from(
    home: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    extra: &[PathBuf],
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(home) = &home {
        dirs.push(home.join(".claude"));
    }
    if let Some(dir) = config_dir {
        dirs.push(dir);
    }
    dirs.extend(extra.iter().cloned());
    let mut roots: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        if dir.as_os_str().is_empty() {
            continue;
        }
        // A leading `~` (or
        // a bare one) expands against the home, and without a home to
        // expand against the path passes through unchanged — the root
        // then simply never holds a transcript.
        let dir = {
            let text = dir.to_string_lossy();
            if text == "~" {
                home.clone().unwrap_or(dir)
            } else if let Some(rest) = text.strip_prefix("~/") {
                home.clone().map(|home| home.join(rest)).unwrap_or(dir)
            } else {
                dir
            }
        };
        let projects = dir.join("projects");
        if !roots.contains(&projects) {
            roots.push(projects);
        }
    }
    roots
}

/// The transcript file for a session id, or `None` — the
/// reference's find rule. The id is interpolated into
/// a path, so anything outside `[0-9a-zA-Z-]` is refused outright; the
/// ledger's session ids are opaque strings the proxy never validated.
/// Each root is scanned for any project directory holding
/// `<session-id>.jsonl`; an unlistable root is just a root with no
/// transcripts in it.
fn find_transcript(sid: &str, roots: &[PathBuf]) -> Option<PathBuf> {
    if sid.is_empty() || !sid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return None;
    }
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path().join(format!("{sid}.jsonl"));
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

/// A session's label, from the tail of its transcript — the
/// reference's session-label rule. Only the last
/// `tail_bytes` are read; a read that starts mid-line drops up to the
/// first newline first, because half a record can match the cwd pattern
/// on the wrong string. Never panics: every failure — no transcript, an
/// unreadable one, a torn tail — is `None`, a row without a name, not
/// an error.
pub(crate) fn session_label(sid: &str, roots: &[PathBuf], tail_bytes: u64) -> Option<Label> {
    let path = find_transcript(sid, roots)?;
    let file = std::fs::File::open(&path).ok()?;
    let size = file.metadata().ok()?.len();
    let start = size.saturating_sub(tail_bytes);
    let mut handle = file;
    if start > 0 && handle.seek(SeekFrom::Start(start)).is_err() {
        return None;
    }
    let mut bytes = vec![0u8; (size - start) as usize];
    let read = handle.read(&mut bytes).ok()?;
    bytes.truncate(read);
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if start > 0
        && let Some(first_nl) = text.find('\n')
    {
        text.drain(..=first_nl);
    }
    label_from_tail(&text)
}

/// The label opencode's own session store carries: its SQLite db has
/// a `session_v2` table with `id`, `title`, and `directory` — the same
/// facts the claude transcripts provide, in a queryable table, no tail
/// scanning at all. Read-only and lock-tolerant: a busy or missing
/// store is no-label, never an error (invariant 6's spirit — the
/// label machinery must never fail a dashboard).
pub(crate) fn opencode_label(sid: &str, db: &Path) -> Option<Label> {
    use rusqlite::OpenFlags;
    let conn = rusqlite::Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    // A tiny busy timeout: opencode holds WAL locks while writing; a
    // label is not worth waiting on — miss this refresh, hit the next.
    let _ = conn.busy_timeout(std::time::Duration::from_millis(250));
    let (title, directory): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT title, directory FROM session_v2 WHERE id = ?1",
            [sid],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()?;
    (title.is_some() || directory.is_some()).then_some(Label {
        cwd: directory,
        title,
        prompt: None,
    })
}

/// The opencode store's location: `$XDG_DATA_HOME/opencode/opencode.db`
/// (the data-home default `~/.local/share/opencode/opencode.db`).
pub(crate) fn opencode_db_default() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    let data_home = match std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(home).join(".local/share"),
    };
    Some(data_home.join("opencode").join("opencode.db"))
}

/// The label a transcript tail carries. Only the few small records
/// wanted are parsed —
/// the rest of the tail is mostly tool results, and parsing a megabyte
/// of them for every session every two seconds would be the whole cost
/// of the view — and the scan runs newest-first so the LATEST of each
/// kind wins: the generated title is regenerated as a session drifts.
/// A line that merely QUOTES a record's shape (a tool result embedding
/// `"type":"ai-title"` as escaped text) never matches the marker, and a
/// line that carries the marker but fails to parse — a torn record —
/// stands in for nothing; the scan keeps going.
fn label_from_tail(text: &str) -> Option<Label> {
    // The raw field values, newest-first; `None` keeps the scan going,
    // exactly the reference's keep-until-set rule (a record whose field
    // is missing or JSON null
    // never wins, and a non-string value wins only to clean to `None`).
    let mut name: Option<serde_json::Value> = None;
    let mut title: Option<serde_json::Value> = None;
    let mut prompt: Option<serde_json::Value> = None;
    let mut cwd: Option<String> = None;
    for line in text.split('\n').rev() {
        if line.is_empty() {
            continue;
        }
        if name.is_none()
            && let Some(found) = record_field(line, "\"type\":\"agent-name\"", "agentName")
        {
            name = Some(found);
        }
        if title.is_none()
            && let Some(found) = record_field(line, "\"type\":\"ai-title\"", "aiTitle")
        {
            title = Some(found);
        }
        if prompt.is_none()
            && let Some(found) = record_field(line, "\"type\":\"last-prompt\"", "lastPrompt")
        {
            prompt = Some(found);
        }
        if cwd.is_none()
            && let Some(found) = cwd_of(line)
        {
            cwd = Some(found);
        }
        if name.is_some() && title.is_some() && prompt.is_some() && cwd.is_some() {
            break;
        }
    }
    let label = Label {
        cwd: clean(cwd),
        title: clean_value(name.as_ref()).or_else(|| clean_value(title.as_ref())),
        prompt: clean_value(prompt.as_ref()),
    };
    (label.cwd.is_some() || label.title.is_some() || label.prompt.is_some()).then_some(label)
}

/// One line's named-record field:
/// the line must carry the exact marker substring — the cheap
/// pre-filter that keeps a full JSON parse off most lines — and then
/// parse, and the field must be present and not JSON null. `None`
/// keeps the scan going: a marker that fails to parse is a torn record,
/// and a parsed record without the field (a tool result quoting the
/// marker's text) is not the record at all.
fn record_field(line: &str, marker: &str, field: &str) -> Option<serde_json::Value> {
    if !line.contains(marker) {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(line).ok()?;
    match parsed.get(field) {
        Some(value) if !value.is_null() => Some(value.clone()),
        _ => None,
    }
}

/// The first `"cwd":"…"` value on the line, unescaped (the
/// `/"cwd":"((?:[^"\\]|\\.)*)"/` capture,
/// re-quoted and JSON-parsed so escapes resolve). Scans the raw bytes:
/// a UTF-8 continuation byte is never `"` or `\`, so multibyte content
/// cannot end the capture early. A capture that does not close before
/// the line ends (a torn record) or one whose escapes are not valid
/// JSON stands in for nothing — `None` keeps the scan going.
fn cwd_of(line: &str) -> Option<String> {
    const MARK: &str = "\"cwd\":\"";
    let bytes = line.as_bytes();
    let start = line.find(MARK)? + MARK.len();
    let mut end = None;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2, // an escape is always two bytes of capture
            b'"' => {
                end = Some(i);
                break;
            }
            _ => i += 1,
        }
    }
    let end = end?;
    let quoted = format!("\"{}\"", &line[start..end]);
    serde_json::from_str(&quoted).ok()
}

/// `clean`: a non-empty string with its
/// whitespace runs collapsed to single spaces; anything else (a
/// non-string field, a blank one) is `None`, never an empty label.
fn clean(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// [`clean`] over a raw JSON value: only a string ever survives.
fn clean_value(value: Option<&serde_json::Value>) -> Option<String> {
    clean(value.and_then(|value| value.as_str()).map(str::to_owned))
}

/// A working directory short enough for a column (the
/// reference's short-dir rule). A worktree's own name is usually
/// a branch or a card code, which says nothing without its repo, so
/// `repo/.../worktrees/x1` reads `repo/x1`.
pub(crate) fn short_dir(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd?;
    let parts: Vec<&str> = cwd.split('/').filter(|part| !part.is_empty()).collect();
    let last = *parts.last()?;
    if parts.len() >= 3 && parts[parts.len() - 2] == "worktrees" {
        let repo = parts[..parts.len() - 2]
            .iter()
            .rfind(|part| **part != ".claude")
            .copied();
        if let Some(repo) = repo {
            return Some(format!("{repo}/{last}"));
        }
    }
    Some(last.to_owned())
}

/// The dashboard's label state: the transcript roots resolved once at
/// startup, plus the per-refresh resolution cache.
///
/// [`Labels::resolve`] reads a session's transcript tail at most once
/// per refresh (the loop's display tick) and caches the answer — `None`
/// included, so a session with no name is not re-probed per row —
/// the reference dashboard's economics: a 2-second refresh re-reads each
/// tail
/// (a title that regenerates mid-session stays current) and never pays
/// twice within one frame. [`Labels::start_refresh`] is that tick.
pub(crate) struct Labels {
    /// The `projects/` roots to look in, resolved once.
    roots: Vec<PathBuf>,
    /// The opencode session store, when its default location exists.
    opencode_db: Option<PathBuf>,
    /// Labels resolved this refresh, keyed by session id.
    resolved: HashMap<String, Option<Label>>,
}

impl Labels {
    /// The label state over resolved roots (see [`transcript_roots`])
    /// and the opencode store's default location.
    pub(crate) fn new(roots: Vec<PathBuf>) -> Self {
        Labels {
            roots,
            opencode_db: opencode_db_default().filter(|db| db.is_file()),
            resolved: HashMap::new(),
        }
    }

    /// The test shape: roots and an explicit opencode db.
    #[cfg(test)]
    fn with_opencode_db(roots: Vec<PathBuf>, db: PathBuf) -> Self {
        Labels {
            roots,
            opencode_db: Some(db),
            resolved: HashMap::new(),
        }
    }

    /// A new display tick: the next [`Labels::resolve`] reads the
    /// transcripts again. Per-refresh, not per-render — the snapshot a
    /// refresh builds carries the labels, so the renders between ticks
    /// never touch the filesystem at all.
    pub(crate) fn start_refresh(&mut self) {
        self.resolved.clear();
    }

    /// The session's label, reading its transcript tail once per
    /// refresh, then opencode's session store. `None` when neither
    /// carries one — a row without a name, never an error. The two
    /// id shapes never overlap (claude's UUIDs vs opencode's `ses_…`),
    /// so the order is a formality, not a precedence.
    pub(crate) fn resolve(&mut self, sid: &str) -> Option<Label> {
        if let Some(hit) = self.resolved.get(sid) {
            return hit.clone();
        }
        let label = session_label(sid, &self.roots, TAIL_BYTES).or_else(|| {
            self.opencode_db
                .as_deref()
                .and_then(|db| opencode_label(sid, db))
        });
        self.resolved.insert(sid.to_owned(), label.clone());
        label
    }
}

/// A path checked against an id that must never leave its directory
/// (the fixture layout below hands the ids around as path pieces).
#[cfg(test)]
fn projects_of(root: &std::path::Path, dir: &str, sid: &str) -> PathBuf {
    let path = root.join(dir);
    std::fs::create_dir_all(&path).expect("create the project dir");
    path.join(format!("{sid}.jsonl"))
}

#[cfg(test)]
mod tests {
    //! The reference transcript-label cases, ported — plus the end-to-end
    //! path over scratch transcript layouts built from the checked-in
    //! fixtures, the tail-only discipline, and the cache economics.
    //!
    //! Env is never touched here: [`super::roots_from`] is the pure
    //! core of the env-reading [`super::transcript_roots`], so the
    //! root-resolution cases pass home/config-dir values as data, the
    //! reference test's own env-as-data trick.

    use std::path::{Path, PathBuf};

    use super::{
        Labels, TAIL_BYTES, find_transcript, label_from_tail, roots_from, session_label, short_dir,
    };
    use serde_json::json;

    /// A fresh scratch directory under /tmp/opencode, unique per call.
    fn scratch(name: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = PathBuf::from("/tmp/opencode").join(format!(
            "toker-labels-{}-{}-{}",
            std::process::id(),
            name,
            n
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    /// A checked-in transcript fixture's text.
    fn fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/transcripts")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read fixture {path:?}: {error}"))
    }

    // ── label_from_tail: the reference cases, ported ────────────────

    #[test]
    fn the_latest_record_wins_and_a_quoted_tool_result_is_not_a_record() {
        let tail = [
            json!({"type": "ai-title", "aiTitle": "Old title"}).to_string(),
            json!({"type": "user", "cwd": "/old/place", "message": {"content": "x"}}).to_string(),
            json!({"type": "last-prompt", "lastPrompt": "fix the\n  thing"}).to_string(),
            json!({"type": "ai-title", "aiTitle": "New title"}).to_string(),
            json!({"type": "user", "cwd": "/home/u/code/repo", "message": {"content": "y"}})
                .to_string(),
            // A tool result quoting a title record's shape — as escaped
            // text inside the line, not as a record of its own.
            json!({"type": "user", "message": {"content": [
                {"type": "tool_result", "content": "\"type\":\"ai-title\""}
            ]}})
            .to_string(),
        ]
        .join("\n");
        let label = label_from_tail(&tail).expect("a label");
        assert_eq!(
            label.title.as_deref(),
            Some("New title"),
            "latest title not taken"
        );
        assert_eq!(
            label.cwd.as_deref(),
            Some("/home/u/code/repo"),
            "latest cwd not taken"
        );
        assert_eq!(
            label.prompt.as_deref(),
            Some("fix the thing"),
            "prompt not collapsed"
        );
    }

    #[test]
    fn a_deliberate_name_beats_a_generated_one() {
        let tail = [
            json!({"type": "agent-name", "agentName": "named"}).to_string(),
            json!({"type": "ai-title", "aiTitle": "gen"}).to_string(),
        ]
        .join("\n");
        assert_eq!(
            label_from_tail(&tail).expect("a label").title.as_deref(),
            Some("named"),
            "generated title beat the agent name"
        );
    }

    #[test]
    fn nothing_usable_is_null_not_an_empty_label() {
        // Bad input never throws: a tail without a usable record is
        // no label at all, never an empty one.
        assert_eq!(
            label_from_tail(&json!({"type": "assistant"}).to_string()),
            None,
            "empty tail produced a label"
        );
        assert_eq!(label_from_tail(""), None, "empty text produced a label");
        assert_eq!(
            label_from_tail(r#"{"type":"ai-title","aiTi"#),
            None,
            "torn line produced a label"
        );
        // A cwd whose value is complete on a torn line still reads —
        // the cwd pattern matches the raw line, not the parsed record.
        assert_eq!(
            label_from_tail(r#"{"type":"user","cwd":"/home/u/code/repo","mess"#)
                .expect("the torn line's complete cwd reads")
                .cwd
                .as_deref(),
            Some("/home/u/code/repo")
        );
    }

    #[test]
    fn a_directory_is_named_by_its_repo_and_a_worktree_by_both() {
        assert_eq!(
            short_dir(Some("/home/u/.workhorse/repos/org/bliti/worktrees/x1")).as_deref(),
            Some("bliti/x1"),
            "worktree not named by repo"
        );
        assert_eq!(
            short_dir(Some("/home/u/code/repo/.claude/worktrees/feat")).as_deref(),
            Some("repo/feat"),
            "claude worktree not named by repo"
        );
        assert_eq!(
            short_dir(Some("/home/u/code/repo")).as_deref(),
            Some("repo"),
            "plain dir mangled"
        );
        assert_eq!(short_dir(Some("/")), None, "no dir produced a name");
        assert_eq!(short_dir(None), None, "no dir produced a name");
    }

    // ── roots and lookup ───────────────────────────────────────────

    #[test]
    fn roots_carry_projects_dedupe_and_expand_tilde() {
        // "Roots are deduplicated and
        // empties dropped": the home default and CLAUDE_CONFIG_DIR
        // naming the same place collapse, `~` expands against the
        // home, and an empty root is dropped.
        let roots = roots_from(
            Some(PathBuf::from("/home/u")),
            Some(PathBuf::from("/home/u/.claude")),
            &[PathBuf::from("~/a"), PathBuf::new(), PathBuf::from("/b")],
        );
        assert_eq!(
            roots,
            vec![
                PathBuf::from("/home/u/.claude/projects"),
                PathBuf::from("/home/u/a/projects"),
                PathBuf::from("/b/projects"),
            ]
        );
        // Without a home there is no ~/.claude root and nothing for `~`
        // to expand against — the rest still look.
        let roots = roots_from(None, None, &[PathBuf::from("~/x"), PathBuf::from("/b")]);
        assert_eq!(
            roots,
            vec![PathBuf::from("~/x/projects"), PathBuf::from("/b/projects")]
        );
        // A bare `~` expands to the home itself.
        let roots = roots_from(Some(PathBuf::from("/home/u")), None, &[PathBuf::from("~")]);
        assert_eq!(
            roots,
            vec![
                PathBuf::from("/home/u/.claude/projects"),
                PathBuf::from("/home/u/projects"),
            ]
        );
    }

    #[test]
    fn an_id_cannot_leave_its_directory() {
        assert_eq!(
            find_transcript("../../etc/passwd", &[PathBuf::from("/tmp")]),
            None
        );
        assert_eq!(find_transcript("", &[PathBuf::from("/tmp")]), None);
        assert_eq!(
            find_transcript("has spaces", &[PathBuf::from("/tmp")]),
            None
        );
        assert_eq!(
            find_transcript("018f2b7c-1111-4a55-9a11-000000000001", &[]),
            None,
            "no roots is no transcript, not a scan of /"
        );
    }

    // ── end to end over the checked-in fixtures ──────────────────────

    /// The realistic tail shapes the reader faces, one fixture each: a
    /// session that drifts (old cwd and title, then new ones), a
    /// session with nothing usable in its tail, a malformed tail line
    /// that must not stand in for the real records, and a final line
    /// truncated mid-write.
    #[test]
    fn fixtures_resolve_over_scratch_transcript_layouts() {
        let home = scratch("fixtures-home");
        let config_dir = scratch("fixtures-cfg");
        let extra = scratch("fixtures-extra");
        let roots = roots_from(
            Some(home.clone()),
            Some(config_dir.clone()),
            std::slice::from_ref(&extra),
        );
        // Each config dir's own projects/ tree, the level Claude Code
        // keeps its transcripts under.
        let home_projects = home.join(".claude").join("projects");
        let cfg_projects = config_dir.join("projects");
        let extra_projects = extra.join("projects");
        // The same sid under two roots: the first root in the order
        // wins (the scan reads roots in order and stops at the first
        // hit).
        let write = |projects: &Path, slug: &str, sid: &str, text: &str| {
            let path = super::projects_of(projects, slug, sid);
            std::fs::write(&path, text).expect("write the transcript");
            path
        };

        // A session whose transcript lives in $CLAUDE_CONFIG_DIR's
        // tree: the newest cwd and title win, and the tool result
        // quoting a marker's text is not a record.
        let sid = "018f2b7c-1111-4a55-9a11-000000000001";
        write(
            &cfg_projects,
            "-home-u-code-old-place",
            sid,
            &fixture("labeled.jsonl"),
        );
        let label = session_label(sid, &roots, TAIL_BYTES).expect("a label");
        assert_eq!(
            label.cwd.as_deref(),
            Some("/home/u/code/toker"),
            "newest cwd wins"
        );
        assert_eq!(
            label.title.as_deref(),
            Some("TUI session labels from transcripts"),
            "newest title wins"
        );
        assert_eq!(
            label.prompt.as_deref(),
            Some("port the transcript reader from the predecessor")
        );

        // A session with nothing usable: no label, never an empty one.
        let sid = "018f2b7c-2222-4a55-9a22-000000000002";
        write(
            &cfg_projects,
            "-home-u-code-quiet",
            sid,
            &fixture("neither.jsonl"),
        );
        assert_eq!(session_label(sid, &roots, TAIL_BYTES), None);

        // Malformed tail lines: a torn record and a torn cwd must not
        // stand in for the complete records before them.
        let sid = "018f2b7c-3333-4a55-9a33-000000000003";
        write(
            &cfg_projects,
            "-home-u-code-toker",
            sid,
            &fixture("malformed.jsonl"),
        );
        let label = session_label(sid, &roots, TAIL_BYTES).expect("a label");
        assert_eq!(
            label.cwd.as_deref(),
            Some("/home/u/code/toker"),
            "the torn cwd never wins"
        );
        assert_eq!(
            label.title.as_deref(),
            Some("The good title"),
            "the torn title never wins"
        );
        assert_eq!(label.prompt.as_deref(), Some("the good prompt"));

        // A final line truncated mid-write, with no trailing newline:
        // its complete cwd value still reads (the cwd pattern works on
        // the raw line), and the complete records before it stand.
        let sid = "018f2b7c-4444-4a55-9a44-000000000004";
        write(
            &extra_projects,
            "-home-u-code-toker",
            sid,
            &fixture("truncated.jsonl"),
        );
        let label = session_label(sid, &roots, TAIL_BYTES).expect("a label");
        assert_eq!(
            label.cwd.as_deref(),
            Some("/home/u/code/toker"),
            "the truncated final line's complete cwd reads"
        );
        assert_eq!(label.title.as_deref(), Some("Still standing"));

        // The first root holding a transcript wins: the same sid under
        // the home default and the extra root resolves from the former.
        let sid = "018f2b7c-5555-4a55-9a55-000000000005";
        write(
            &home_projects,
            "-home-u",
            sid,
            &json!({"type": "ai-title", "aiTitle": "from the home root"}).to_string(),
        );
        write(
            &extra_projects,
            "-home-u",
            sid,
            &json!({"type": "ai-title", "aiTitle": "from the extra root"}).to_string(),
        );
        let label = session_label(sid, &roots, TAIL_BYTES).expect("a label");
        assert_eq!(label.title.as_deref(), Some("from the home root"));

        // An unreadable transcript — here a directory named like one —
        // degrades to no label, never a panic.
        let sid = "018f2b7c-6666-4a55-9a66-000000000006";
        let path = super::projects_of(&extra_projects, "-home-u-code-toker", sid);
        std::fs::create_dir(&path).expect("a directory named like a transcript");
        assert_eq!(session_label(sid, &roots, TAIL_BYTES), None);
    }

    // ── the tail-only discipline ────────────────────────────────────

    #[test]
    fn the_tail_read_never_reaches_a_canary_before_its_window() {
        // More than TAIL_BYTES of tool results between a canary at the
        // start and the real records near the end. The canary is an
        // agent-name record: a full-file read would find it, and an
        // agent name beats every generated title wherever it sits — so
        // the label resolving to the tail's own title pins the byte
        // budget, not just the newest-wins rule. The cwd pair near the
        // end pins newest-wins within the tail.
        let root = scratch("tail-window");
        let sid = "018f2b7c-7777-4a55-9a77-000000000007";
        let path = super::projects_of(&root, "-home-u-code-toker", sid);

        let mut text = String::new();
        text.push_str(
            &json!({"type": "agent-name", "agentName": "CANARY from before the window"})
                .to_string(),
        );
        text.push('\n');
        text.push_str(
            &json!({"type": "user", "cwd": "/canary/old-world",
                    "message": {"role": "user", "content": "before the window"}})
            .to_string(),
        );
        text.push('\n');
        // Padding past the byte budget: tool results, the shape that
        // makes a transcript large.
        let padding = json!({
            "type": "user",
            "message": {"content": [
                {"type": "tool_result", "content":
                    "the whole file, far too long to be a label record, padded to a transcript's real turn size"}
            ]}
        })
        .to_string();
        while text.len() < TAIL_BYTES as usize + 128 * 1024 {
            text.push_str(&padding);
            text.push('\n');
        }
        text.push_str(
            &json!({"type": "user", "cwd": "/home/u/code/toker-older",
                    "message": {"role": "user", "content": "the older recent turn"}})
            .to_string(),
        );
        text.push('\n');
        text.push_str(
            &json!({"type": "user", "cwd": "/home/u/code/toker",
                    "message": {"role": "user", "content": "the newest turn"}})
            .to_string(),
        );
        text.push('\n');
        text.push_str(&json!({"type": "ai-title", "aiTitle": "Old title"}).to_string());
        text.push('\n');
        text.push_str(&json!({"type": "ai-title", "aiTitle": "New title"}).to_string());
        text.push('\n');
        text.push_str(&json!({"type": "last-prompt", "lastPrompt": "newest prompt"}).to_string());
        text.push('\n');
        std::fs::write(&path, &text).expect("write the transcript");
        assert!(
            std::fs::metadata(&path).expect("stat").len() > TAIL_BYTES,
            "the fixture must be past the byte budget for the test to mean anything"
        );

        let label = session_label(sid, std::slice::from_ref(&root), TAIL_BYTES).expect("a label");
        assert_eq!(
            label.cwd.as_deref(),
            Some("/home/u/code/toker"),
            "newest cwd wins"
        );
        assert_eq!(
            label.title.as_deref(),
            Some("New title"),
            "newest title wins"
        );
        assert_eq!(label.prompt.as_deref(), Some("newest prompt"));
        assert!(
            !matches!(label.title.as_deref(), Some(t) if t.contains("CANARY")),
            "the pre-window agent name was read: {label:?}"
        );
    }

    #[test]
    fn a_smaller_tail_budget_excludes_what_sits_before_its_window() {
        // The tail window is the last N bytes. With
        // the canary inside a big window but outside a small one, the
        // two budgets must resolve differently — the small one clips
        // the canary, the big one reads it.
        let root = scratch("tail-budget");
        let sid = "018f2b7c-8888-4a55-9a88-000000000008";
        let path = super::projects_of(&root, "-home-u-code-toker", sid);

        let mut text = String::new();
        text.push_str(&json!({"type": "agent-name", "agentName": "canary"}).to_string());
        text.push('\n');
        let padding = json!({
            "type": "user",
            "message": {"content": [{"type": "tool_result", "content": "p"}]}
        })
        .to_string();
        while text.len() < 900 {
            text.push_str(&padding);
            text.push('\n');
        }
        let real = [
            json!({"type": "user", "cwd": "/home/u/code/toker"}).to_string(),
            json!({"type": "ai-title", "aiTitle": "the real title"}).to_string(),
        ]
        .join("\n");
        text.push_str(&real);
        text.push('\n');
        std::fs::write(&path, &text).expect("write the transcript");

        // The small window covers the real records only: the canary
        // sits before its start (and the torn first line it lands mid-
        // way into is dropped with it).
        let small = real.len() as u64 + 40;
        let label = session_label(sid, std::slice::from_ref(&root), small).expect("a label");
        assert_eq!(label.cwd.as_deref(), Some("/home/u/code/toker"));
        assert_eq!(label.title.as_deref(), Some("the real title"));

        // The whole file reads the canary too — the agent name beats
        // the generated title wherever it sits.
        let label = session_label(sid, std::slice::from_ref(&root), TAIL_BYTES).expect("a label");
        assert_eq!(label.title.as_deref(), Some("canary"));
        assert_eq!(label.cwd.as_deref(), Some("/home/u/code/toker"));
    }

    // ── the cache ───────────────────────────────────────────────────

    #[test]
    fn labels_resolve_once_per_refresh_and_read_again_on_the_next() {
        let root = scratch("cache");
        let sid = "018f2b7c-9999-4a55-9a99-000000000009";
        let path = super::projects_of(&root, "-home-u-code-toker", sid);
        std::fs::write(
            &path,
            json!({"type": "ai-title", "aiTitle": "first title"}).to_string(),
        )
        .expect("write the transcript");

        let mut labels = Labels::new(vec![root.clone()]);
        assert_eq!(
            labels.resolve(sid).and_then(|label| label.title),
            Some("first title".to_owned())
        );
        // The session's title regenerates mid-refresh: within THIS
        // refresh the first resolution stands — one read, not two.
        std::fs::write(
            &path,
            json!({"type": "ai-title", "aiTitle": "regenerated title"}).to_string(),
        )
        .expect("rewrite the transcript");
        assert_eq!(
            labels.resolve(sid).and_then(|label| label.title),
            Some("first title".to_owned()),
            "a second resolve in one refresh must not re-read"
        );

        // The next display tick re-reads — the
        // per-refresh pass.
        labels.start_refresh();
        assert_eq!(
            labels.resolve(sid).and_then(|label| label.title),
            Some("regenerated title".to_owned())
        );

        // A miss caches too: the transcript appearing mid-refresh is
        // invisible until the next tick.
        let late = "018f2b7c-aaaa-4a55-9aaa-00000000000a";
        assert_eq!(labels.resolve(late), None);
        std::fs::write(
            super::projects_of(&root, "-home-u-code-toker", late),
            json!({"type": "ai-title", "aiTitle": "late title"}).to_string(),
        )
        .expect("write the late transcript");
        assert_eq!(
            labels.resolve(late),
            None,
            "the miss is cached for the refresh"
        );
        labels.start_refresh();
        assert_eq!(
            labels.resolve(late).and_then(|label| label.title),
            Some("late title".to_owned())
        );
    }
    // ── the opencode store source ────────────────────────────────

    /// A scratch opencode store: the session_v2 shape the query reads.
    fn opencode_scratch(root: &Path) -> PathBuf {
        let dir = root.join("opencode");
        std::fs::create_dir_all(&dir).expect("dir");
        let db = dir.join("opencode.db");
        let conn = rusqlite::Connection::open(&db).expect("open");
        conn.execute_batch(
            "CREATE TABLE session_v2 (
                 id TEXT PRIMARY KEY, title TEXT, directory TEXT
             );",
        )
        .expect("schema");
        db
    }

    #[test]
    fn the_opencode_store_labels_sessions_by_title_and_directory() {
        let root = scratch("opencode-labels");
        let db = opencode_scratch(&root);
        {
            let conn = rusqlite::Connection::open(&db).expect("open");
            conn.execute(
                "INSERT INTO session_v2 (id, title, directory) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    "ses_op_1",
                    "Models-API fetch and cache",
                    "/home/user/code/rust/toker"
                ],
            )
            .expect("seed");
            conn.execute(
                "INSERT INTO session_v2 (id, title, directory) VALUES (?1, NULL, ?2)",
                rusqlite::params!["ses_op_untitled", "/home/user/code/somewhere"],
            )
            .expect("seed");
        }

        let labeled = super::opencode_label("ses_op_1", &db).expect("labeled");
        assert_eq!(labeled.title.as_deref(), Some("Models-API fetch and cache"));
        assert_eq!(labeled.cwd.as_deref(), Some("/home/user/code/rust/toker"));
        assert_eq!(labeled.prompt, None);

        // Untitled but placed: the directory is the label.
        let untitled = super::opencode_label("ses_op_untitled", &db).expect("labeled");
        assert_eq!(untitled.title, None);
        assert_eq!(untitled.cwd.as_deref(), Some("/home/user/code/somewhere"));

        // Misses and failures are no-labels, never errors.
        assert_eq!(super::opencode_label("ses_op_missing", &db), None);
        let bogus = root.join("not-a.db");
        assert_eq!(super::opencode_label("ses_op_1", &bogus), None);
    }

    #[test]
    fn resolve_falls_through_transcripts_to_the_opencode_store() {
        // No transcript roots at all: the opencode store answers.
        let root = scratch("opencode-fallthrough");
        let db = opencode_scratch(&root);
        {
            let conn = rusqlite::Connection::open(&db).expect("open");
            conn.execute(
                "INSERT INTO session_v2 (id, title, directory) VALUES (?1, ?2, ?3)",
                rusqlite::params!["ses_ft_1", "Wake hold ping timers", "/w/repo"],
            )
            .expect("seed");
        }
        let mut labels = super::Labels::with_opencode_db(Vec::new(), db);
        let hit = labels.resolve("ses_ft_1").expect("labeled");
        assert_eq!(hit.title.as_deref(), Some("Wake hold ping timers"));
        assert_eq!(hit.cwd.as_deref(), Some("/w/repo"));
        // The refresh cache: a second resolve never re-reads (the map
        // is keyed; this pins the fall-through result cached the same).
        assert_eq!(labels.resolve("ses_ft_1"), Some(hit));
    }

    /// The real machine's store, read-only: the newest sessions the
    /// ledger names resolve to real titles and directories. Manual
    /// run only (the store is the user's live data; the suite must
    /// never depend on it existing).
    #[test]
    #[ignore = "reads the real opencode store; run with --ignored"]
    fn the_real_opencode_store_resolves_real_sessions() {
        let Some(db) = super::opencode_db_default() else {
            panic!("no opencode store at the default location");
        };
        let conn =
            rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("open the real store read-only");
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM session_v2 ORDER BY time_created DESC LIMIT 3")
            .expect("prepare")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .filter_map(Result::ok)
            .collect();
        for id in &ids {
            let label = super::opencode_label(id, &db);
            println!(
                "{id}: {}",
                match &label {
                    Some(label) => format!(
                        "{} · {}",
                        super::short_dir(label.cwd.as_deref()).unwrap_or_else(|| "-".into()),
                        label.title.as_deref().unwrap_or("(untitled)")
                    ),
                    None => "no label".to_owned(),
                }
            );
        }
    }
}
