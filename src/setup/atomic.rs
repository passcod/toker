//! Atomic JSON file writes — the opencode.json lesson, mechanised.
//!
//! `toker setup` patches config files it does not own (claude's
//! settings.json, opencode's opencode.json): a write that leaves one of
//! those half-updated breaks the tool that reads it mid-rewrite, and the
//! incident that taught this lesson was an opencode.json truncated by a
//! non-atomic write. Every write here is:
//!
//! 1. **temp-write in the SAME DIRECTORY as the target** — `rename(2)`
//!    is only atomic within one filesystem, and a cross-device rename
//!    fails with `EXDEV` *after* the write "succeeded": the temp file
//!    lands happily in `/tmp`, the rename silently targets another
//!    device, and the real file never changes. Same directory means
//!    same filesystem by construction; a `/tmp` temp cannot promise
//!    that. (A cross-device scenario cannot be constructed portably in
//!    tests, so this is documented here and pinned by
//!    [`temp_path_lands_beside_the_target`] instead.)
//! 2. **fsync** — the bytes reach the disk before the rename, not after
//!    it;
//! 3. **re-parse and compare** — the temp is read back from disk and
//!    must round-trip to the exact value we meant to write, so
//!    truncation or corruption is caught BEFORE the rename, never
//!    after it;
//! 4. **rename** — the one atomic step: a reader sees either the whole
//!    old file or the whole new one, never a partial state. A failure
//!    at any earlier step removes the temp and leaves the target
//!    untouched.
//!
//! Pre-existing content is preserved by construction: the JSON
//! [`Value`] round-trip keeps every key and — with the crate's
//! `preserve_order` — their order, so a patch changes only the key it
//! names. A file that already holds JSON that does not parse is refused
//! with a clear error, never clobbered, and neither is a non-UTF-8
//! file.
//!
//! Symlinks: a patch goes THROUGH the link. The target is resolved with
//! `fs::canonicalize` — following the file's own link and every
//! symlinked directory on the way — the temp is written beside the REAL
//! file, and the rename replaces the real file, leaving the link
//! itself intact. A settings.json symlinked into a synced vault edits
//! the vault file, which is the only correct behaviour: replacing the
//! link would silently break the sync. A symlink whose target does not
//! exist is an error, not a fresh file, for the same reason.

use std::fs;
use std::fs::Permissions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, bail};
use serde_json::Value;

/// Write `value` to `path` atomically (see the module docs for the
/// temp-write / fsync / re-parse / rename sequence). Unlike
/// [`atomic_patch_json`] this is a whole-file write: the result on disk
/// is exactly `value`, nothing else — pre-existing keys are NOT merged
/// in.
pub fn atomic_write_json(path: &Path, value: &Value) -> anyhow::Result<()> {
    let bytes = json_bytes(value)?;
    atomic_write_bytes(path, &bytes, None, |_temp, written| {
        verify_json(written, value)
    })
}

/// Read the JSON at `path` (starting from `{}` when it is absent), apply
/// `patch`, and write the result back atomically. The patch sees — and
/// may refuse — the existing value; a patch that bails changes nothing.
/// The written file keeps every pre-existing key and its order
/// (`preserve_order`), formatted two-space pretty with a trailing
/// newline: the shape every hand-edited settings file on the reference
/// machine has, so a re-serialised fixture is byte-for-byte its old
/// self except the patched key.
pub fn atomic_patch_json<F>(path: &Path, patch: F) -> anyhow::Result<()>
where
    F: FnOnce(&mut Value) -> anyhow::Result<()>,
{
    let target = resolve_target(path)?;
    let mut value = read_json_start(&target, path)?;
    patch(&mut value).with_context(|| format!("patching {}", path.display()))?;
    let bytes = json_bytes(&value)?;
    atomic_write_bytes(&target, &bytes, None, |_temp, written| {
        verify_json(written, &value)
    })
}

/// [`atomic_patch_json`] for a patch that is re-run on a schedule: when
/// the patch leaves the value as it was, nothing is written and `false`
/// comes back. Claude Code hot-reloads its settings into every running
/// session, so a daily sync that found nothing new must not touch the
/// file (an absent file a no-op patch leaves absent).
pub fn atomic_patch_json_if_changed<F>(path: &Path, patch: F) -> anyhow::Result<bool>
where
    F: FnOnce(&mut Value) -> anyhow::Result<()>,
{
    let target = resolve_target(path)?;
    let before = read_json_start(&target, path)?;
    let mut value = before.clone();
    patch(&mut value).with_context(|| format!("patching {}", path.display()))?;
    if value == before {
        return Ok(false);
    }
    let bytes = json_bytes(&value)?;
    atomic_write_bytes(&target, &bytes, None, |_temp, written| {
        verify_json(written, &value)
    })?;
    Ok(true)
}

/// The write primitive every setup module shares (JSON, TOML, and the
/// shell-rc text alike): temp file beside `target`, `fresh_mode` on it
/// when the target is being created, fsync, read back, hand the bytes
/// to `verify` (the re-parse-and-compare step — re-loading the file at
/// `temp` through the real parser is allowed and preferred), then
/// rename over the target. The target's existing mode is preserved: the
/// temp's default creation mode must never loosen a 0600 config by the
/// rename. On any failure the temp is removed — no partial states, no
/// leftovers.
pub(crate) fn atomic_write_bytes<V>(
    path: &Path,
    bytes: &[u8],
    fresh_mode: Option<u32>,
    verify: V,
) -> anyhow::Result<()>
where
    V: FnOnce(&Path, &[u8]) -> anyhow::Result<()>,
{
    let target = resolve_target(path)?;
    let temp = temp_path(&target);
    let attempt = (|| -> anyhow::Result<()> {
        let mut file =
            fs::File::create(&temp).with_context(|| format!("creating {}", temp.display()))?;
        if let Some(mode) = fresh_mode {
            // Before the first byte: a file that may hold a literal key
            // is never briefly world-readable while the write is in
            // flight.
            fs::set_permissions(&temp, Permissions::from_mode(mode))
                .context("setting the fresh file's mode")?;
        }
        file.write_all(bytes)
            .with_context(|| format!("writing {}", temp.display()))?;
        file.sync_all().context("syncing the temp file")?;
        drop(file);
        let written = fs::read(&temp).context("reading the temp file back")?;
        verify(&temp, &written)?;
        if let Ok(existing) = fs::metadata(&target) {
            fs::set_permissions(&temp, existing.permissions())
                .context("preserving the target's mode")?;
        }
        fs::rename(&temp, &target)
            .with_context(|| format!("renaming {} over {}", temp.display(), target.display()))?;
        Ok(())
    })();
    if attempt.is_err() {
        // No partial states: a failed write leaves nothing behind.
        let _ = fs::remove_file(&temp);
    }
    attempt
}

/// The file a write must land in: symlinks followed (patch THROUGH the
/// link — see the module docs), the parent directory created when the
/// path is fresh, and a fresh path resolved through its (possibly
/// symlinked) parent too.
fn resolve_target(path: &Path) -> anyhow::Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                // A dangling link is an error, not a fresh file:
                // renaming over the link would silently break whatever
                // synced it.
                return fs::canonicalize(path).with_context(|| {
                    format!(
                        "{} is a symlink whose target does not exist — refusing to replace the link",
                        path.display()
                    )
                });
            }
            fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
            let parent = fs::canonicalize(parent)
                .with_context(|| format!("resolving {}", parent.display()))?;
            let name = path
                .file_name()
                .with_context(|| format!("{} names no file", path.display()))?;
            Ok(parent.join(name))
        }
        Err(error) => Err(error).with_context(|| format!("examining {}", path.display())),
    }
}

/// The existing JSON at `target`, or `{}` when absent. A file that is
/// present but does not parse is an error naming `display` (the path
/// the caller knows, which may be a symlink) — never a clobber.
fn read_json_start(target: &Path, display: &Path) -> anyhow::Result<Value> {
    match fs::read_to_string(target) {
        Ok(text) => serde_json::from_str(&text).with_context(|| {
            format!(
                "{} holds JSON that does not parse — refusing to touch it",
                display.display()
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(Value::Object(serde_json::Map::new()))
        }
        Err(error) => Err(error).with_context(|| format!("reading {}", display.display())),
    }
}

/// The value as pretty two-space JSON with a trailing newline — the
/// byte shape of the reference machine's hand-edited settings files, so
/// a rewrite of a file already in that shape changes only the patched
/// key's line.
fn json_bytes(value: &Value) -> anyhow::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value).context("serialising the JSON")?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// The re-parse-and-compare step: the bytes read back from disk must
/// parse to exactly the intended value.
fn verify_json(written: &[u8], intended: &Value) -> anyhow::Result<()> {
    let reparsed: Value = serde_json::from_slice(written).context("re-parsing the written JSON")?;
    if &reparsed != intended {
        bail!("the written JSON does not round-trip to the intended value — refusing to rename");
    }
    Ok(())
}

/// The temp file's path: in the target's OWN directory (the
/// same-filesystem requirement — see the module docs), hidden
/// (dot-prefixed, so no directory scanner ever sees a half-written
/// config), and unique per process (pid + counter).
fn temp_path(target: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let dir = target.parent().unwrap_or_else(|| Path::new(""));
    dir.join(format!(".{name}.toker-{}-{n}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::setup::test_dir;

    /// A fixture in the house byte shape (pretty + trailing newline),
    /// so a patch's output is byte-comparable against the same shape.
    fn write_pretty(path: &Path, value: &Value) -> Vec<u8> {
        let bytes = json_bytes(value).expect("serialise fixture");
        fs::write(path, &bytes).expect("write fixture");
        bytes
    }

    #[test]
    fn fresh_file_write_creates_it() {
        let path = test_dir("write-fresh").join("settings.json");
        let value = json!({"a": 1, "nested": {"b": [true, null, "x"]}});
        atomic_write_json(&path, &value).expect("write");
        let bytes = fs::read(&path).expect("read back");
        assert_eq!(bytes, json_bytes(&value).expect("serialise"));
        assert_eq!(
            bytes.last(),
            Some(&b'\n'),
            "POSIX text file, trailing newline"
        );
    }

    #[test]
    fn fresh_file_in_a_missing_directory_is_created() {
        let path = test_dir("write-mkdir").join("deep/er/settings.json");
        atomic_write_json(&path, &json!({"a": 1})).expect("write");
        let read: Value =
            serde_json::from_slice(&fs::read(&path).expect("read back")).expect("parse");
        assert_eq!(read, json!({"a": 1}));
    }

    #[test]
    fn patch_starts_from_an_empty_object_when_the_file_is_absent() {
        let path = test_dir("patch-fresh").join("settings.json");
        atomic_patch_json(&path, |value| {
            let map = value.as_object_mut().expect("starts as an object");
            map.insert("k".to_owned(), json!(42));
            Ok(())
        })
        .expect("patch");
        let read: Value =
            serde_json::from_slice(&fs::read(&path).expect("read back")).expect("parse");
        assert_eq!(read, json!({"k": 42}));
    }

    #[test]
    fn unknown_keys_are_preserved_byte_for_byte_except_the_patched_key() {
        // The matrix the frontends' real files motivated: a fixture
        // with every JSON shape a settings file carries (nested
        // objects, arrays of objects, unicode, numbers, bools), patched
        // at one nested key, must come back byte-for-byte itself except
        // that one key's line.
        let dir = test_dir("patch-bytes");
        let path = dir.join("settings.json");
        let value = json!({
            "$schema": "https://example/x.json",
            "env": {"ANTHROPIC_BASE_URL": "https://api.example", "OTHER": "1"},
            "permissions": {"allow": ["Bash(thing *)"], "defaultMode": "auto"},
            "hooks": [{"hooks": [{"type": "command", "async": true}]}],
            "list": [1, 2.5, null],
            "unicode": "héllo → ✓",
            "flag": true,
        });
        write_pretty(&path, &value);
        atomic_patch_json(&path, |value| {
            value["env"]["ANTHROPIC_BASE_URL"] = json!("http://127.0.0.1:18123");
            Ok(())
        })
        .expect("patch");

        let mut expected = value.clone();
        expected["env"]["ANTHROPIC_BASE_URL"] = json!("http://127.0.0.1:18123");
        let expected_bytes = json_bytes(&expected).expect("serialise");
        assert_eq!(
            fs::read(&path).expect("read back"),
            expected_bytes,
            "every other byte survives: order, nesting, spacing, all of it"
        );
    }

    #[test]
    fn key_order_is_preserved_and_a_new_key_goes_last() {
        let dir = test_dir("patch-order");
        let path = dir.join("settings.json");
        let value = json!({"b": 1, "a": 2});
        write_pretty(&path, &value);
        atomic_patch_json(&path, |value| {
            let map = value.as_object_mut().expect("object");
            map.insert("z".to_owned(), json!(3));
            map.insert("b".to_owned(), json!(9)); // existing key: value changes, position must not
            Ok(())
        })
        .expect("patch");
        let read: Value =
            serde_json::from_slice(&fs::read(&path).expect("read back")).expect("parse");
        assert_eq!(read, json!({"b": 9, "a": 2, "z": 3}));
        assert_eq!(
            read.as_object().expect("object").keys().collect::<Vec<_>>(),
            ["b", "a", "z"],
            "preserve_order: existing keys keep their places, a new one appends"
        );
    }

    #[test]
    fn invalid_json_is_refused_never_clobbered() {
        let dir = test_dir("patch-invalid");
        let path = dir.join("settings.json");
        fs::write(&path, b"{ this is not json").expect("write broken fixture");
        let error =
            atomic_patch_json(&path, |_value| Ok(())).expect_err("invalid JSON must be refused");
        let chain = format!("{error:#}");
        assert!(
            chain.contains("does not parse") && chain.contains("refusing"),
            "a clear refusal: {chain}"
        );
        assert_eq!(
            fs::read(&path).expect("read back"),
            b"{ this is not json",
            "the broken file is untouched"
        );
    }

    #[test]
    fn an_empty_file_is_refused_not_read_as_absent() {
        // An empty file is present-but-unparseable, not absent content:
        // writing {} over it would be a clobber of a file whose state
        // we do not understand.
        let dir = test_dir("patch-empty");
        let path = dir.join("settings.json");
        fs::write(&path, b"").expect("write empty fixture");
        let error =
            atomic_patch_json(&path, |_value| Ok(())).expect_err("an empty file must be refused");
        assert!(format!("{error:#}").contains("does not parse"));
        assert_eq!(fs::read(&path).expect("read back"), b"");
    }

    #[test]
    fn non_utf8_files_are_refused_not_clobbered() {
        let dir = test_dir("patch-utf8");
        let path = dir.join("settings.json");
        fs::write(&path, b"\xff\xfe{}").expect("write non-utf8 fixture");
        let error =
            atomic_patch_json(&path, |_value| Ok(())).expect_err("non-utf8 must be refused");
        assert!(format!("{error:#}").contains("reading"));
        assert_eq!(fs::read(&path).expect("read back"), b"\xff\xfe{}");
    }

    #[test]
    fn a_failing_patch_leaves_the_file_untouched_and_no_temp_behind() {
        let dir = test_dir("patch-fail");
        let path = dir.join("settings.json");
        let original = write_pretty(&path, &json!({"keep": "me"}));
        let error = atomic_patch_json(&path, |_value| {
            bail!("the patch refuses this shape");
        })
        .expect_err("the patch's refusal propagates");
        assert!(format!("{error:#}").contains("refuses this shape"));
        assert_eq!(fs::read(&path).expect("read back"), original);
        assert_eq!(
            fs::read_dir(&dir).expect("list dir").count(),
            1,
            "only the target: a failed patch leaves no temp file"
        );
    }

    #[test]
    fn a_verify_failure_leaves_nothing_behind() {
        // The re-parse-and-compare step is the last gate before the
        // rename; its failure must remove the temp and never touch the
        // target (fresh or existing).
        let dir = test_dir("verify-fail");
        let path = dir.join("toker.toml");
        let error = atomic_write_bytes(&path, b"some bytes", None, |_temp, _written| {
            bail!("the re-parse refused these bytes")
        })
        .expect_err("a verify failure propagates");
        assert!(format!("{error:#}").contains("re-parse refused"));
        assert!(!path.exists(), "the fresh target was never created");
        assert_eq!(
            fs::read_dir(&dir).expect("list dir").count(),
            0,
            "no temp left behind"
        );

        let existing = dir.join("existing.json");
        fs::write(&existing, b"original").expect("write existing");
        atomic_write_bytes(&existing, b"new bytes", None, |_temp, _written| {
            bail!("nope")
        })
        .expect_err("a verify failure on an existing file propagates");
        assert_eq!(fs::read(&existing).expect("read back"), b"original");
        assert_eq!(fs::read_dir(&dir).expect("list dir").count(), 1);
    }

    #[test]
    fn a_symlinked_file_is_patched_through_to_the_real_file() {
        // The machine's shape to support: a settings file symlinked
        // into a synced vault. The patch edits the vault file, the link
        // stays a link, and the temp never lands in the link's
        // directory.
        let vault = test_dir("symlink-vault");
        let links = test_dir("symlink-links");
        let real = vault.join("settings.json");
        let link = links.join("settings.json");
        write_pretty(
            &real,
            &json!({"env": {"ANTHROPIC_BASE_URL": "https://api.example"}}),
        );
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        atomic_patch_json(&link, |value| {
            value["env"]["ANTHROPIC_BASE_URL"] = json!("http://127.0.0.1:18123");
            Ok(())
        })
        .expect("patch through the link");

        let patched: Value = serde_json::from_slice(&fs::read(&real).expect("read the real file"))
            .expect("parse the real file");
        assert_eq!(
            patched["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:18123"
        );
        assert!(
            fs::symlink_metadata(&link)
                .expect("the link still exists")
                .file_type()
                .is_symlink(),
            "the link itself is never replaced"
        );
        assert_eq!(
            fs::read_link(&link).expect("read the link"),
            real,
            "the link points where it always did"
        );
        assert_eq!(
            fs::read_dir(&links).expect("list the link's dir").count(),
            1,
            "the temp landed beside the REAL file, not beside the link"
        );
        assert_eq!(
            fs::read_dir(&vault).expect("list the vault dir").count(),
            1,
            "and it was renamed away from there"
        );
    }

    #[test]
    fn a_fresh_file_in_a_symlinked_directory_lands_in_the_real_directory() {
        let real_dir = test_dir("symlink-dir-real");
        let link_dir = test_dir("symlink-dir-link").join("config");
        std::os::unix::fs::symlink(&real_dir, &link_dir).expect("symlink the directory");
        let via_link = link_dir.join("settings.json");

        atomic_write_json(&via_link, &json!({"a": 1})).expect("write through the dir link");

        assert!(
            real_dir.join("settings.json").exists(),
            "the real dir holds the file"
        );
        assert!(
            !link_dir.join(".settings.json.toker-999-999.tmp").exists(),
            "no temp left in the linked path"
        );
        let read: Value =
            serde_json::from_slice(&fs::read(real_dir.join("settings.json")).expect("read back"))
                .expect("parse");
        assert_eq!(read, json!({"a": 1}));
    }

    #[test]
    fn a_dangling_symlink_is_refused() {
        let dir = test_dir("dangling");
        let target = dir.join("elsewhere.json");
        let link = dir.join("settings.json");
        std::os::unix::fs::symlink(&target, &link).expect("symlink to nowhere");

        let error = atomic_patch_json(&link, |_value| Ok(()))
            .expect_err("a dangling symlink must be refused");
        let chain = format!("{error:#}");
        assert!(
            chain.contains("symlink") && chain.contains("refusing"),
            "the refusal explains itself: {chain}"
        );
        assert!(
            fs::symlink_metadata(&link)
                .expect("the link survives")
                .file_type()
                .is_symlink(),
            "the dangling link is not replaced by a file"
        );
        assert!(!target.exists(), "nothing was created at the dead target");
    }

    #[test]
    fn the_existing_files_mode_is_preserved_and_fresh_mode_applies() {
        let dir = test_dir("modes");
        let existing = dir.join("settings.json");
        write_pretty(&existing, &json!({"a": 1}));
        fs::set_permissions(&existing, Permissions::from_mode(0o600)).expect("chmod 600");
        atomic_patch_json(&existing, |value| {
            value
                .as_object_mut()
                .expect("object")
                .insert("b".to_owned(), json!(2));
            Ok(())
        })
        .expect("patch");
        let mode = fs::metadata(&existing)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the 0600 config is not loosened by the rename"
        );

        let fresh = dir.join("toker.toml");
        atomic_write_bytes(&fresh, b"x = 1\n", Some(0o600), |_temp, written| {
            anyhow::ensure!(written == b"x = 1\n");
            Ok(())
        })
        .expect("write fresh");
        let mode = fs::metadata(&fresh).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "fresh_mode applies to a new file");
    }

    #[test]
    fn temp_path_lands_beside_target() {
        // The cross-device scenario itself cannot be constructed
        // portably in a test (see the module docs); what CAN be pinned
        // is the property that makes it impossible: the temp is always
        // a sibling of the target, whatever the target's directory.
        let target = Path::new("/any/where/settings.json");
        let temp = temp_path(target);
        assert_eq!(temp.parent(), Some(Path::new("/any/where")));
        assert!(
            temp.file_name()
                .expect("a name")
                .to_str()
                .expect("utf-8")
                .starts_with(".settings.json.toker-"),
            "hidden, and names its target: {temp:?}"
        );
        assert_ne!(temp_path(target), temp_path(target), "unique per call");
        let bare = temp_path(Path::new("settings.json"));
        assert_eq!(
            bare.parent(),
            Some(Path::new("")),
            "a bare name stays local"
        );
    }
}
