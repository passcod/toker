//! Claude Code's `/model` picker rows for openrouter models, built from
//! openrouter's live models listing by configured rules.
//!
//! Claude Code reads extra picker rows from `modelPicker` in
//! `~/.claude/settings.json` (user settings only; a project's settings
//! cannot carry it). Picking a row sends its `model` verbatim, so a row's
//! model is `openrouter/<id>`, which toker routes to openrouter's
//! Anthropic-compatible endpoint (see `docs/internals/routing.md`, which
//! also records why this is not `/v1/models` discovery).
//!
//! The rows are not a fixed list: each rule names globs over openrouter
//! ids and keeps the newest matches, so a new version of a model replaces
//! the old one on the next sync without anyone editing anything. This
//! module is pure: the listing comes in as JSON, the rows go out as JSON,
//! and the `behaves_as` resolution is the caller's closure. Writing the
//! rows into the settings file is `setup::patchers::patch_model_picker`.

use std::collections::HashSet;

use anyhow::{Context, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::catalog::windows::verified_ids;
use crate::middleware::models::{family_of, newest_in_family};
use crate::store::ModelEntry;

/// The prefix that routes a model to openrouter on the Anthropic wire,
/// and marks a picker row as toker's: rows whose `model` starts with it
/// are replaced on every sync, every other row is the user's.
pub const ROW_PREFIX: &str = "openrouter/";

/// One picker rule (`[[providers.openrouter.picker]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PickerRule {
    /// Globs over openrouter model ids; a model matching any is a candidate.
    #[serde(rename = "match")]
    pub matches: Vec<String>,
    /// Globs that take a candidate back out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// The model whose client-side handling Claude Code applies to these
    /// rows (its `behavesAs`): a family name (`opus`, `sonnet`, `haiku`,
    /// `fable`), resolved at sync time, or a full Claude model id, passed
    /// through. Claude Code does not offer a row for a model it does not
    /// know without one.
    pub behaves_as: String,
    /// An openrouter variant suffix (`:floor`, `:nitro`) appended to the
    /// id the row sends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// How many of the newest matches (by the listing's `created`) the
    /// rule offers.
    #[serde(default = "default_keep", skip_serializing_if = "is_default_keep")]
    pub keep: usize,
}

fn default_keep() -> usize {
    1
}

fn is_default_keep(keep: &usize) -> bool {
    *keep == 1
}

impl PickerRule {
    fn new(matches: &[&str], exclude: &[&str], behaves_as: &str) -> PickerRule {
        PickerRule {
            matches: matches.iter().map(|glob| (*glob).to_owned()).collect(),
            exclude: exclude.iter().map(|glob| (*glob).to_owned()).collect(),
            behaves_as: behaves_as.to_owned(),
            variant: None,
            keep: 1,
        }
    }

    /// Refuse a rule that could never produce a row, at config load
    /// rather than at the first sync.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.matches.is_empty() {
            bail!("a picker rule needs at least one `match` glob");
        }
        globs(&self.matches)?;
        globs(&self.exclude)?;
        if self.behaves_as.trim().is_empty() {
            bail!("picker rule {:?}: `behaves_as` is empty", self.matches);
        }
        if self.keep == 0 {
            bail!("picker rule {:?}: `keep` must be at least 1", self.matches);
        }
        if let Some(variant) = &self.variant
            && (!variant.starts_with(':') || variant.len() == 1)
        {
            bail!(
                "picker rule {:?}: `variant` must look like `:floor`, got {variant:?}",
                self.matches
            );
        }
        Ok(())
    }
}

fn globs(patterns: &[String]) -> anyhow::Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).with_context(|| format!("picker glob {pattern:?}"))?);
    }
    Ok(builder.build()?)
}

/// The built-in rules, used when the config names none: one flagship
/// and at most one fast model per well-known lab, the Auto Router, and,
/// only when `with_anthropic`, Anthropic's four families. With an
/// anthropic backend enabled, Claude Code's built-in lineup already
/// reaches those, and a second row per model would only crowd the
/// picker. Checked against openrouter's listing of 2026-10-06 (the
/// vendored fixture, and the snapshot of the rows it produces).
pub fn default_rules(with_anthropic: bool) -> Vec<PickerRule> {
    let mut rules = vec![
        PickerRule::new(&["openai/gpt-*-sol"], &["*-pro"], "opus"),
        PickerRule::new(&["openai/gpt-*-luna"], &["*-pro"], "haiku"),
        PickerRule::new(&["google/gemini-*-flash"], &[], "sonnet"),
        PickerRule::new(&["moonshotai/kimi-k*"], &["*-code", "*-thinking"], "sonnet"),
        PickerRule::new(
            &["z-ai/glm-*"],
            &["*-flash*", "*v*", "*-turbo", "*-prime"],
            "sonnet",
        ),
        PickerRule::new(&["z-ai/glm-*-flash"], &[], "haiku"),
        PickerRule::new(&["deepseek/deepseek-v*-pro*"], &[], "sonnet"),
        PickerRule::new(&["deepseek/deepseek-v*-flash*"], &["*-vision*"], "haiku"),
        PickerRule::new(&["qwen/qwen*-max*"], &["*-prime"], "sonnet"),
        PickerRule::new(&["minimax/minimax-m*"], &[], "sonnet"),
        PickerRule::new(&["openrouter/auto"], &[], "sonnet"),
    ];
    if with_anthropic {
        for family in ["opus", "sonnet", "haiku", "fable"] {
            rules.push(PickerRule::new(
                &[&format!("anthropic/claude-{family}-*")],
                &[],
                family,
            ));
        }
    }
    rules
}

/// The built-in rules as `toker.toml` text, to copy and edit
/// (`toker picker defaults`). Both sets are printed, the anthropic one
/// marked, because which applies depends on the config they land in.
pub fn defaults_toml() -> String {
    #[derive(Serialize)]
    struct Block<'a> {
        picker: &'a [PickerRule],
    }
    #[derive(Serialize)]
    struct Providers<'a> {
        openrouter: Block<'a>,
    }
    #[derive(Serialize)]
    struct File<'a> {
        providers: Providers<'a>,
    }
    let render = |rules: &[PickerRule]| {
        toml::to_string(&File {
            providers: Providers {
                openrouter: Block { picker: rules },
            },
        })
        .expect("picker rules serialise")
    };
    let all = default_rules(true);
    let without = default_rules(false);
    format!(
        "# The built-in picker rules. A `picker` list in toker.toml replaces\n\
         # them wholesale; `picker = []` offers nothing.\n\n{}\n\
         # Only when neither anthropic_sub nor anthropic_api is enabled:\n\n{}",
        render(&without),
        render(&all[without.len()..])
    )
}

/// One model from openrouter's listing, as far as the picker reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    pub id: String,
    pub name: String,
    pub created: i64,
    pub context_length: Option<u64>,
    /// USD per token, as listed; `-1` for a router's variable price.
    pub prompt_price: Option<f64>,
    pub completion_price: Option<f64>,
}

/// The listing cut to what Claude Code can use: models that take tools
/// (it is unusable without them) and text input. Ids already carrying a
/// `:variant` are dropped, because the variant is a rule's choice; a
/// listed variant would otherwise compete with its own base model.
pub fn eligible(listing: &Value) -> anyhow::Result<Vec<Listed>> {
    let data = listing
        .get("data")
        .and_then(Value::as_array)
        .context("openrouter models listing has no `data` array")?;
    Ok(data
        .iter()
        .filter(|model| {
            let has = |field: &str, item: &str| {
                model
                    .pointer(field)
                    .and_then(Value::as_array)
                    .is_some_and(|items| items.iter().any(|value| value.as_str() == Some(item)))
            };
            has("/supported_parameters", "tools") && has("/architecture/input_modalities", "text")
        })
        .filter_map(|model| {
            let id = model.get("id")?.as_str()?;
            if id.contains(':') {
                return None;
            }
            let price = |field: &str| {
                model
                    .pointer(field)
                    .and_then(Value::as_str)
                    .and_then(|price| price.parse::<f64>().ok())
            };
            Some(Listed {
                id: id.to_owned(),
                name: model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_owned(),
                created: model.get("created").and_then(Value::as_i64).unwrap_or(0),
                context_length: model.get("context_length").and_then(Value::as_u64),
                prompt_price: price("/pricing/prompt"),
                completion_price: price("/pricing/completion"),
            })
        })
        .collect())
}

/// One `modelPicker.options` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub model: String,
    pub label: String,
    pub description: String,
    pub behaves_as: String,
}

impl Row {
    /// The row as Claude Code's settings spell it.
    pub fn to_json(&self) -> Value {
        json!({
            "model": self.model,
            "label": self.label,
            "description": self.description,
            "behavesAs": self.behaves_as,
        })
    }
}

/// The rows the rules produce from the eligible models, with a warning
/// per rule that offered nothing. A model matched by two rules is the
/// first one's. `behaves_as` maps a rule's value to the id Claude Code
/// gets; `None` skips the rule's rows (a family nothing knows).
pub fn rows(
    rules: &[PickerRule],
    models: &[Listed],
    behaves_as: &dyn Fn(&str) -> Option<String>,
) -> anyhow::Result<(Vec<Row>, Vec<String>)> {
    let mut taken = HashSet::new();
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    for rule in rules {
        let include = globs(&rule.matches)?;
        let exclude = globs(&rule.exclude)?;
        let mut candidates: Vec<&Listed> = models
            .iter()
            .filter(|model| {
                include.is_match(&model.id)
                    && !exclude.is_match(&model.id)
                    && !taken.contains(model.id.as_str())
            })
            .collect();
        // Newest first; the id breaks a tie so the order never depends on
        // the listing's.
        candidates.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
        candidates.truncate(rule.keep);
        if candidates.is_empty() {
            warnings.push(format!(
                "picker rule {:?} matches no openrouter model",
                rule.matches
            ));
            continue;
        }
        let Some(behaves) = behaves_as(&rule.behaves_as) else {
            warnings.push(format!(
                "picker rule {:?}: no known model for behaves_as {:?}; set a full model id",
                rule.matches, rule.behaves_as
            ));
            continue;
        };
        for model in candidates {
            taken.insert(model.id.as_str());
            let variant = rule.variant.as_deref().unwrap_or("");
            out.push(Row {
                model: format!("{ROW_PREFIX}{}{variant}", model.id),
                label: match variant.strip_prefix(':') {
                    Some(name) => format!("{} ({name})", model.name),
                    None => model.name.clone(),
                },
                description: description(model),
                behaves_as: behaves.clone(),
            });
        }
    }
    Ok((out, warnings))
}

/// `OpenRouter · 262k ctx · $0.60/$2.50 per Mtok`, from the listing's
/// own figures: provider facts shown as given, never a toker estimate.
fn description(model: &Listed) -> String {
    let mut parts = vec!["OpenRouter".to_owned()];
    if let Some(context) = model.context_length.filter(|context| *context > 0) {
        parts.push(format!("{} ctx", tokens(context)));
    }
    match (model.prompt_price, model.completion_price) {
        (Some(input), Some(output)) if input < 0.0 || output < 0.0 => {
            parts.push("variable price".to_owned());
        }
        (Some(input), Some(output)) if input == 0.0 && output == 0.0 => {
            parts.push("free".to_owned());
        }
        (Some(input), Some(output)) => parts.push(format!(
            "${}/${} per Mtok",
            per_million(input),
            per_million(output)
        )),
        _ => {}
    }
    parts.join(" \u{b7} ")
}

fn tokens(count: u64) -> String {
    if count >= 1_000_000 {
        // Two places, trimmed: one would round 1,050,000 up to "1.1M".
        let millions = format!("{:.2}", count as f64 / 1_000_000.0);
        format!("{}M", millions.trim_end_matches('0').trim_end_matches('.'))
    } else {
        format!("{}k", (count as f64 / 1000.0).round() as u64)
    }
}

fn per_million(per_token: f64) -> String {
    format!("{:.2}", per_token * 1_000_000.0)
}

/// A `behaves_as` value as Claude Code should get it. A value with no
/// `-` is a family, resolved to the newest model of it that the ledger
/// has learned (a model Claude Code itself has sent, so one it knows),
/// else the newest the hand-verified catalogue names. Anything else is
/// a model id, passed through.
pub fn resolve_behaves_as(value: &str, learned: &[ModelEntry]) -> Option<String> {
    let value = value.trim();
    if value.contains('-') {
        return Some(value.to_owned());
    }
    newest_in_family(learned, value).or_else(|| {
        verified_ids()
            .filter_map(|id| {
                let family = family_of(id)?;
                (family.name == value).then_some((family.version, id))
            })
            .max()
            .map(|(_, id)| id.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing() -> Value {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/openrouter/models-2026-10-06.json"
        );
        serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
    }

    fn verified_only(value: &str) -> Option<String> {
        resolve_behaves_as(value, &[])
    }

    fn render(rows: &[Row]) -> String {
        rows.iter()
            .map(|row| {
                format!(
                    "{} | {} | {} | {}",
                    row.model, row.label, row.description, row.behaves_as
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_default_rules_against_the_vendored_listing() {
        let models = eligible(&listing()).expect("eligible");
        let (rows, warnings) = rows(&default_rules(true), &models, &verified_only).expect("rows");
        assert_eq!(warnings, Vec::<String>::new());
        insta::assert_snapshot!(render(&rows));
    }

    #[test]
    fn the_anthropic_rules_join_only_when_asked() {
        let models = eligible(&listing()).expect("eligible");
        let (rows, _) = rows(&default_rules(false), &models, &verified_only).expect("rows");
        assert!(
            rows.iter()
                .all(|row| !row.model.starts_with("openrouter/anthropic/"))
        );
        assert!(
            rows.iter()
                .any(|row| row.model == "openrouter/openrouter/auto")
        );
    }

    fn model(id: &str, created: i64) -> Listed {
        Listed {
            id: id.to_owned(),
            name: id.to_owned(),
            created,
            context_length: Some(262_144),
            prompt_price: Some(0.000_000_6),
            completion_price: Some(0.000_002_5),
        }
    }

    #[test]
    fn keep_takes_the_newest_and_a_variant_rides_the_id_and_label() {
        let models = [
            model("lab/m-1", 1),
            model("lab/m-3", 3),
            model("lab/m-2", 2),
        ];
        let mut rule = PickerRule::new(&["lab/m-*"], &[], "claude-sonnet-5");
        rule.keep = 2;
        rule.variant = Some(":floor".to_owned());
        let (rows, _) = rows(&[rule], &models, &verified_only).expect("rows");
        assert_eq!(
            rows.iter()
                .map(|row| row.model.as_str())
                .collect::<Vec<_>>(),
            ["openrouter/lab/m-3:floor", "openrouter/lab/m-2:floor"]
        );
        assert_eq!(rows[0].label, "lab/m-3 (floor)");
        assert_eq!(
            rows[0].description,
            "OpenRouter \u{b7} 262k ctx \u{b7} $0.60/$2.50 per Mtok"
        );
        assert_eq!(
            rows[0].behaves_as, "claude-sonnet-5",
            "a full id passes through"
        );
    }

    #[test]
    fn a_model_is_the_first_matching_rule_s() {
        let models = [model("lab/m-1", 1), model("lab/m-2", 2)];
        let first = PickerRule::new(&["lab/*"], &[], "claude-opus-5");
        let second = PickerRule::new(&["lab/m-*"], &[], "claude-haiku-4-5");
        let (rows, _) = rows(&[first, second], &models, &verified_only).expect("rows");
        assert_eq!(rows[0].model, "openrouter/lab/m-2");
        assert_eq!(rows[0].behaves_as, "claude-opus-5");
        assert_eq!(
            rows[1].model, "openrouter/lab/m-1",
            "the second rule's next newest"
        );
    }

    #[test]
    fn a_rule_that_matches_nothing_warns_and_offers_nothing() {
        let models = [model("lab/m-1", 1)];
        let rule = PickerRule::new(&["other/*"], &[], "sonnet");
        let (rows, warnings) = rows(&[rule], &models, &verified_only).expect("rows");
        assert!(rows.is_empty());
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn an_unknown_family_skips_its_rows_rather_than_guessing() {
        let models = [model("lab/m-1", 1)];
        let rule = PickerRule::new(&["lab/*"], &[], "nonesuch");
        let (rows, warnings) = rows(&[rule], &models, &verified_only).expect("rows");
        assert!(rows.is_empty());
        assert!(warnings[0].contains("nonesuch"), "{warnings:?}");
    }

    #[test]
    fn prices_read_as_listed() {
        let mut router = model("openrouter/auto", 1);
        router.prompt_price = Some(-1.0);
        router.completion_price = Some(-1.0);
        router.context_length = Some(2_000_000);
        assert_eq!(
            description(&router),
            "OpenRouter \u{b7} 2M ctx \u{b7} variable price"
        );
        let mut free = model("lab/free", 1);
        free.prompt_price = Some(0.0);
        free.completion_price = Some(0.0);
        free.context_length = Some(1_048_576);
        assert_eq!(
            description(&free),
            "OpenRouter \u{b7} 1.05M ctx \u{b7} free"
        );
    }

    #[test]
    fn the_listing_is_cut_to_tool_taking_text_models_without_variants() {
        let listing = json!({"data": [
            {"id": "a/tools", "supported_parameters": ["tools"],
             "architecture": {"input_modalities": ["text"]}},
            {"id": "a/no-tools", "supported_parameters": ["temperature"],
             "architecture": {"input_modalities": ["text"]}},
            {"id": "a/image-only", "supported_parameters": ["tools"],
             "architecture": {"input_modalities": ["image"]}},
            {"id": "a/tools:free", "supported_parameters": ["tools"],
             "architecture": {"input_modalities": ["text"]}},
        ]});
        let ids: Vec<String> = eligible(&listing)
            .expect("eligible")
            .into_iter()
            .map(|model| model.id)
            .collect();
        assert_eq!(ids, ["a/tools"]);
    }

    #[test]
    fn families_resolve_learned_first_then_verified() {
        assert_eq!(
            resolve_behaves_as("opus", &[]).as_deref(),
            Some("claude-opus-5-5"),
            "the newest verified opus"
        );
        assert_eq!(
            resolve_behaves_as("claude-opus-4-8", &[]).as_deref(),
            Some("claude-opus-4-8")
        );
    }

    #[test]
    fn rules_validate_at_load() {
        let mut rule = PickerRule::new(&["lab/*"], &[], "sonnet");
        assert!(rule.validate().is_ok());
        rule.keep = 0;
        assert!(rule.validate().is_err());
        rule.keep = 1;
        rule.variant = Some("floor".to_owned());
        assert!(rule.validate().is_err(), "a variant without its colon");
        rule.variant = None;
        rule.matches = vec!["lab/[".to_owned()];
        assert!(rule.validate().is_err(), "an unparseable glob");
    }

    #[test]
    fn the_printed_defaults_parse_back_as_rules() {
        #[derive(Deserialize)]
        struct File {
            providers: Providers,
        }
        #[derive(Deserialize)]
        struct Providers {
            openrouter: Block,
        }
        #[derive(Deserialize)]
        struct Block {
            picker: Vec<PickerRule>,
        }
        let text = defaults_toml();
        let (first, second) = text
            .split_once("# Only when")
            .expect("the anthropic set is marked");
        let parsed: File = toml::from_str(first).expect("the main set parses");
        assert_eq!(parsed.providers.openrouter.picker, default_rules(false));
        let second = second.split_once('\n').expect("marker line").1;
        let parsed: File = toml::from_str(second).expect("the anthropic set parses");
        assert_eq!(parsed.providers.openrouter.picker.len(), 4);
    }
}
