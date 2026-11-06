//! `toker.toml` writing — the wizard's config half: read-merge, change,
//! rewrite, all atomic.
//!
//! **The decision this module implements** (plan: "Setup wizard" — "No
//! config files written by hand unless wanted; `toker.toml` exists for
//! hand-editing later"): the wizard writes the file toker fully
//! understands. [`Config::load_from`](crate::config::Config::load_from)
//! reads and **validates** the existing file first, the wizard's chosen
//! changes are applied to that resolved `Config`, and the whole config
//! is serialised with serde `toml` — deliberately not `toml_edit`:
//! this is toker-owned machine config, not a hand-curated document.
//!
//! The file carries providers and the listener, not routing. The database
//! owns routing, so the legacy routing fields are read (a first activation
//! compiles them) but never written back.
//!
//! What that trades away, deliberately, with the tests below pinning
//! each behaviour so it stays a decision and not an accident:
//!
//! - **Comments, blank lines, and hand formatting do not survive a
//!   rewrite.** Hand-editing is still supported — a hand edit is
//!   READ-MERGED on the next wizard run (and validated by the same
//!   loader), so nothing hand-set is lost unless the wizard is told to
//!   change it; only the prose around it is.
//! - **Normalisations become explicit**: absent keys are written with
//!   their resolved defaults (a wizard-written file says
//!   `port = 18123` rather than implying it), `~`-paths are written
//!   expanded (they already expanded at load), upstream URLs in their
//!   parsed normalised form.
//!   The re-load-compare step proves each of these round-trips to the
//!   same resolved config.
//! - **A key this toker does not understand cannot be silently
//!   dropped at all**: the read side is `deny_unknown_fields`, so an
//!   unknown key is a load error and the rewrite REFUSES rather than
//!   running over it — the strictness that makes "the file it fully
//!   understands" safe to say.
//!
//! The write itself is the shared atomic primitive (temp-write beside
//! the target, fsync, re-parse, compare, rename — see [`atomic`]); a
//! fresh file is created 0600 because `toker.toml` is one of the two
//! sanctioned homes for a literal API key, and an existing file's mode
//! is preserved as it is.

use std::path::Path;

use anyhow::{Context, bail};

use crate::config::Config;
use crate::setup::atomic::atomic_write_bytes;

/// Write `toker.toml` at `path`: load and validate whatever is already
/// there (a missing file is the defaults — a first run), apply `changes`
/// to the resolved [`Config`], re-validate, serialise, and put the
/// result down atomically. `changes` may bail to refuse the write
/// (nothing has been touched yet when it does).
///
/// The file holds the providers and the listener only. The legacy routing
/// fields (protocol defaults, model maps, picker rules, `force_newest`,
/// `compact_model`) are read from an existing file so a first activation
/// can compile them, but are never written: the database owns routing, and
/// [`legacy_routing_fields`] says what a rewrite drops. Protocol defaults
/// are therefore not validated here.
pub fn write_config(
    path: &Path,
    changes: impl FnOnce(&mut Config) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut config = Config::load_from(path)
        .with_context(|| format!("reading the existing {}", path.display()))?;
    changes(&mut config).context("applying the wizard's config changes")?;
    config
        .validate_retained()
        .context("the wizard's config changes do not validate")?;
    let text = toml::to_string_pretty(&config.to_owned_file()).context("serialising toker.toml")?;
    let intended = config;
    atomic_write_bytes(path, text.as_bytes(), Some(0o600), move |temp, _written| {
        let reloaded = Config::load_from(temp).context("re-loading the written toker.toml")?;
        if toml::to_string(&reloaded.to_owned_file()).ok()
            != toml::to_string(&intended.to_owned_file()).ok()
        {
            bail!(
                "the written toker.toml does not round-trip to the intended config — \
                 refusing to rename"
            );
        }
        Ok(())
    })
}

/// The legacy routing fields `text` carries: the ones a database-owned
/// rewrite drops, because the active revision holds their compiled form.
/// Unparseable text carries none.
pub fn legacy_routing_fields(text: &str) -> Vec<String> {
    let Ok(table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let mut fields = Vec::new();
    for key in ["default_backend_anthropic", "default_backend_openai_chat"] {
        if table.contains_key(key) {
            fields.push(key.to_owned());
        }
    }
    if let Some(gates) = table.get("gates").and_then(toml::Value::as_table) {
        for key in ["force_newest", "compact_model"] {
            if gates.contains_key(key) {
                fields.push(format!("gates.{key}"));
            }
        }
    }
    if let Some(providers) = table.get("providers").and_then(toml::Value::as_table) {
        for (name, block) in providers {
            let Some(block) = block.as_table() else {
                continue;
            };
            for key in ["model_map", "picker"] {
                if block.contains_key(key) {
                    fields.push(format!("providers.{name}.{key}"));
                }
            }
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use crate::setup::test_dir;

    /// The reference machine's real `toker.toml`, shape and prose
    /// verbatim (comments included — they are the documented trade).
    const REAL_MACHINE_TOML: &str = r#"# toker — local config (this machine). Claude drives the codex sub
# through toker's translation; opencode drives openrouter unchanged.
# The family map (inherited from the predecessor): claude's model names → codex slugs.
default_backend_anthropic = "codex_sub"

[providers.codex_sub.model_map]
"family:opus" = "gpt-5.6-sol"
"family:fable" = "gpt-5.6-sol"
"family:sonnet" = "gpt-5.6-terra"
"family:haiku" = "gpt-5.6-luna"
"#;

    #[test]
    fn policy_owned_write_omits_operational_routing_fields() {
        let dir = test_dir("owned-config");
        let path = dir.join("toker.toml");
        fs::write(&path, REAL_MACHINE_TOML).expect("write fixture");
        write_config(&path, |_| Ok(())).expect("owned write");
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("default_backend_"));
        assert!(!text.contains("model_map"));
        assert!(!text.contains("picker"));
        assert!(!text.contains("compact_model"));
        assert!(!text.contains("force_newest"));
        assert!(text.contains("[providers.codex_sub]"));
    }

    #[test]
    fn round_trips_the_real_file_with_a_change_and_the_documented_trades() {
        let dir = test_dir("roundtrip");
        let path = dir.join("toker.toml");
        fs::write(&path, REAL_MACHINE_TOML).expect("write fixture");

        write_config(&path, |config| {
            anyhow::ensure!(
                config.default_backend_anthropic.as_deref() == Some("codex_sub"),
                "the fixture is read as it stands"
            );
            // A backend is enabled by its block: the flip adds one.
            config.anthropic_sub.get_or_insert_with(Default::default);
            Ok(())
        })
        .expect("write the flip");

        let text = fs::read_to_string(&path).expect("read back");
        // The documented trade, asserted: the hand-written prose does
        // not survive the rewrite...
        assert!(
            !text.contains("# toker — local config"),
            "comments are dropped: the documented trade"
        );
        // ...what the resolver understands is now explicit, defaults
        // included, because the wizard writes the whole file it understands...
        assert!(
            text.contains("port = 18123"),
            "the default port is written explicitly"
        );
        // ...and the routing the database owns is not carried.
        assert!(!text.contains("default_backend_anthropic"));
        assert!(!text.contains("model_map"));

        let reloaded = Config::load_from(&path).expect("the written file loads");
        assert_eq!(reloaded.port, 18_123);
        assert!(reloaded.codex_sub.is_some() && reloaded.anthropic_sub.is_some());
        assert!(
            !text.contains("[providers.openrouter]") && !text.contains("[providers.anthropic_api]"),
            "a disabled backend's block is not written: it would enable it"
        );

        // Idempotence: a no-change re-run rewrites the same bytes.
        let once = fs::read(&path).expect("read once");
        write_config(&path, |_config| Ok(())).expect("no-change rewrite");
        assert_eq!(
            fs::read(&path).expect("read twice"),
            once,
            "byte-stable re-run"
        );
    }

    #[test]
    fn several_backends_without_a_protocol_default_still_load_and_rewrite() {
        let dir = test_dir("ambiguous");
        let path = dir.join("toker.toml");
        fs::write(&path, "[providers.anthropic_api]\n[providers.codex_sub]\n")
            .expect("write fixture");
        write_config(&path, |_| Ok(())).expect("the database owns the defaults");
        let reloaded = Config::load_from(&path).expect("loads");
        assert_eq!(reloaded.default_backend_anthropic, None);
        assert!(
            reloaded.validate().is_err(),
            "routing input still needs one"
        );
    }

    #[test]
    fn legacy_routing_fields_lists_what_a_rewrite_drops() {
        let fields = legacy_routing_fields(
            "default_backend_openai_chat = \"openrouter\"\n\
             [gates]\nforce_newest = false\ncold_min_tokens = 5\n\
             [providers.codex_sub.model_map]\n\"family:opus\" = \"x\"\n\
             [providers.openrouter]\npicker = []\n",
        );
        assert_eq!(
            fields,
            [
                "default_backend_openai_chat",
                "gates.force_newest",
                "providers.codex_sub.model_map",
                "providers.openrouter.picker",
            ]
        );
        assert!(legacy_routing_fields("port = 1\n[providers.openrouter]\n").is_empty());
        assert!(legacy_routing_fields("not toml = = =").is_empty());
    }

    #[test]
    fn understood_keys_are_not_lost_even_when_the_wizard_did_not_ask_about_them() {
        let dir = test_dir("preserve");
        let path = dir.join("toker.toml");
        fs::write(
            &path,
            r#"
port = 19999
awake = false

[gates]
cold_min_tokens = 50000

[providers.openrouter]
upstream = "http://localhost:9/v1"
api_key = "literal-key"

[providers.openai_api]
upstream = "http://localhost:10/v1"
api_key_env = "MY_OPENAI_KEY"

[providers.codex_sub]
auth_path = "~/.codex/auth.json"
"#,
        )
        .expect("write fixture");

        write_config(&path, |config| {
            config.gates.cold_min_tokens = 60_000;
            Ok(())
        })
        .expect("write");

        let reloaded = Config::load_from(&path).expect("loads");
        assert_eq!(reloaded.port, 19_999, "untouched keys keep their values");
        assert!(!reloaded.awake);
        assert_eq!(
            reloaded.gates.cold_min_tokens, 60_000,
            "the asked-for change"
        );
        let openrouter = reloaded.openrouter.as_ref().expect("still enabled");
        assert_eq!(openrouter.api_key.as_deref(), Some("literal-key"));
        assert_eq!(
            openrouter.upstream.as_str(),
            "http://localhost:9/v1",
            "the upstream survived"
        );
        let openai = reloaded.openai_api.as_ref().expect("still enabled");
        assert_eq!(openai.api_key_env, "MY_OPENAI_KEY");
        assert_eq!(openai.upstream.as_str(), "http://localhost:10/v1");
        // The `~` normalisation: auth_path expanded at load, written
        // absolute — semantics identical, bytes deliberately not.
        let expected_auth = PathBuf::from(std::env::var("HOME").expect("tests run with a home"))
            .join(".codex")
            .join("auth.json");
        assert_eq!(
            reloaded
                .codex_sub
                .as_ref()
                .expect("still enabled")
                .auth_path,
            expected_auth
        );
        let text = fs::read_to_string(&path).expect("read back");
        assert!(!text.contains("~/"), "the tilde form is written expanded");
    }

    #[test]
    fn an_unknown_key_refuses_the_rewrite_and_never_clobbers() {
        // The read side is deny_unknown_fields, so "unknown-key loss"
        // cannot happen: a key this toker does not understand is a load
        // error, and the rewrite refuses rather than running over it.
        let dir = test_dir("unknown");
        let path = dir.join("toker.toml");
        fs::write(&path, "prot = 1\n").expect("write fixture");
        let error = write_config(&path, |_config| Ok(())).expect_err("must refuse");
        let chain = format!("{error:#}");
        assert!(
            chain.contains("reading the existing") && chain.contains("prot"),
            "the refusal names the file and the key: {chain}"
        );
        assert_eq!(fs::read(&path).expect("read back"), b"prot = 1\n");
    }

    #[test]
    fn a_change_that_does_not_validate_refuses_and_never_clobbers() {
        let dir = test_dir("invalid");
        let path = dir.join("toker.toml");
        fs::write(&path, "port = 19999\n").expect("write fixture");
        let error = write_config(&path, |config| {
            config.ping_header_name = String::new();
            Ok(())
        })
        .expect_err("must refuse");
        let chain = format!("{error:#}");
        assert!(
            chain.contains("do not validate") && chain.contains("ping_header_name"),
            "{chain}"
        );
        assert_eq!(fs::read(&path).expect("read back"), b"port = 19999\n");
    }

    #[test]
    fn a_broken_existing_file_refuses_and_never_clobbers() {
        let dir = test_dir("broken");
        let path = dir.join("toker.toml");
        fs::write(&path, "port = not-a-number\n").expect("write broken fixture");
        let error = write_config(&path, |_config| Ok(())).expect_err("must refuse");
        assert!(format!("{error:#}").contains("reading the existing"));
        assert_eq!(
            fs::read(&path).expect("read back"),
            b"port = not-a-number\n"
        );
    }

    #[test]
    fn a_fresh_file_gets_the_defaults_plus_the_changes_at_mode_600() {
        let dir = test_dir("fresh");
        let path = dir.join("toker.toml");
        write_config(&path, |config| {
            config.port = 20_000;
            Ok(())
        })
        .expect("write fresh");
        let reloaded = Config::load_from(&path).expect("loads");
        assert_eq!(reloaded.port, 20_000);
        assert_eq!(
            reloaded.default_backend_anthropic, None,
            "defaults intact: a fresh file enables no backend"
        );
        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "a fresh toker.toml may hold a literal key: 0600"
        );
    }

    #[test]
    fn an_existing_files_mode_is_preserved() {
        let dir = test_dir("mode");
        let path = dir.join("toker.toml");
        fs::write(&path, "port = 19999\n").expect("write fixture");
        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod 640");
        write_config(&path, |config| {
            config.port = 20_000;
            Ok(())
        })
        .expect("rewrite");
        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o640,
            "the operator's own mode is not touched"
        );
    }

    #[test]
    fn a_symlinked_toml_is_rewritten_through_the_link() {
        let vault = test_dir("toml-vault");
        let links = test_dir("toml-links");
        let real = vault.join("toker.toml");
        let link = links.join("toker.toml");
        fs::write(&real, REAL_MACHINE_TOML).expect("write the real file");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        write_config(&link, |config| {
            config.port = 20_000;
            Ok(())
        })
        .expect("write through the link");

        let reloaded = Config::load_from(&real).expect("the REAL file loads");
        assert_eq!(reloaded.port, 20_000, "the vault file was patched");
        assert!(
            fs::symlink_metadata(&link)
                .expect("the link survives")
                .file_type()
                .is_symlink(),
            "the link is never replaced"
        );
    }

    #[test]
    fn picker_rules_are_read_but_not_rewritten() {
        let dir = test_dir("picker");
        let path = dir.join("toker.toml");
        fs::write(
            &path,
            "[providers.openrouter]\n\n\
             [[providers.openrouter.picker]]\n\
             match = [\"moonshotai/kimi-k*\"]\n\
             behaves_as = \"sonnet\"\n",
        )
        .expect("write fixture");
        let loaded = Config::load_from(&path).expect("loads");
        assert_eq!(loaded.openrouter.expect("enabled").picker.unwrap().len(), 1);
        write_config(&path, |_| Ok(())).expect("rewrite");
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("picker"), "{text}");
        assert!(text.contains("[providers.openrouter]"));
    }

    #[test]
    fn a_picker_rule_that_cannot_match_is_refused_at_load() {
        let dir = test_dir("picker-bad");
        let path = dir.join("toker.toml");
        fs::write(
            &path,
            "[providers.openrouter]\n\
             [[providers.openrouter.picker]]\n\
             match = [\"lab/[\"]\n\
             behaves_as = \"sonnet\"\n",
        )
        .expect("write fixture");
        let error = Config::load_from(&path).expect_err("an unparseable glob");
        assert!(format!("{error:#}").contains("picker"), "{error:#}");
    }
}
