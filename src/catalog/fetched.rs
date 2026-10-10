//! The fetched model catalogues — the providers' own models listings,
//! cached (plan: Model catalogues — the live counterpart to
//! [`super::windows`]).
//!
//! The hand-verified catalogue is the ceiling source of record, but it
//! can only know what a hand verified. The backends also expose a
//! **models listing** that names, per exact model id, how large that
//! model's context window is:
//!
//! - openrouter: the public `GET {upstream}/models` — `{data: [{id,
//!   context_length, pricing, supported_parameters, …}]}` (no auth;
//!   the transparent forward already serves opencode's own use of it).
//! - openai api: authenticated `GET {upstream}/models` — `{data: [{id,
//!   created, owned_by, …}]}`. The listing is presence metadata only; it
//!   does not declare context windows.
//! - codex: `GET {base}/models?client_version=…` — `{"models":
//!   [{slug, context_window, max_context_window,
//!   supported_reasoning_levels, truncation_policy, …}]}`, the
//!   endpoint the codex CLI itself consults (clean-room note: this
//!   endpoint's shape is that protocol extraction, and the CLI's own
//!   cache discipline — on-disk JSON cache, 300 s TTL, ETag
//!   revalidation — is the pattern this unit's cache follows, on a
//!   dashboard's cadence instead of a client's).
//! - anthropic: `GET {base}/v1/models?limit=1000` — `{data: [{id,
//!   display_name, created_at, max_input_tokens, max_tokens,
//!   capabilities, …}]}`. `max_input_tokens` is the context window.
//!   Earlier versions of the listing carried no window at all, so this
//!   catalogue was once a presence list; an entry whose
//!   `max_input_tokens` is absent, null or zero (the documented example
//!   shows `0`) still parses as presence only. The hand-verified
//!   windows stay authoritative for the models they name.
//!
//! ## Precedence (the reconciliation rule)
//!
//! [`super::windows::resolve_context_window`] consults, in order: the
//! hand-verified catalogue (it encodes beta phases and fixed knowledge
//! a listing cannot), then the fetched ceiling for the row's provider,
//! then a stored declaration, then unknown. A model the hand-verified
//! catalogue KNOWS resolves to its verdict even when that verdict is
//! `Unknown` — an uncaptured beta phase is a decision, not a gap a
//! listing may fill. Every fetched ceiling is a `Declared` verdict: a
//! provider listing is a declaration, not hand-verified source
//! capability.
//!
//! ## Ownership: the daemon writes, the TUI reads
//!
//! One JSON cache file per source under [`cache_dir`]
//! (`$XDG_DATA_HOME/toker/models-cache`). The daemon's background task
//! ([`crate::server`]) refreshes them at startup and every
//! [`CACHE_TTL_MS`]; the TUI reads the same files read-only, on its own
//! cadence (mtime-gated), and never fetches. The cache stores the
//! provider's **raw response** plus `fetched_at_ms` — not a parsed
//! catalogue — so the file stays provider-shaped (comparable with a
//! fresh curl) and the same parse functions run at load: a parse-rule
//! fix applies to cached data with no migration.
//!
//! ## Auth per source (invariant 2)
//!
//! - openrouter's `/models` is public — fetched with no credential.
//! - anthropic's `/v1/models` requires auth. With an `anthropic_api`
//!   key, the daily refresh sends it as `x-api-key`. Without one, toker
//!   holds no anthropic credential, so the daily refresh only reads the
//!   cache, and the fetch instead borrows the subscription bearer of a
//!   request passing through to `anthropic_sub`
//!   (`Server::borrow_catalog_credential`): only while
//!   the catalogue is stale, at most once an hour, and for that one GET.
//!   The bearer is never stored, cached or logged. The subscription
//!   bearer reads the listing with the `oauth-2025-04-20` beta flag
//!   (checked against the live endpoint on 2026-10-06).
//! - codex's `{base}/models` needs the codex bearer: the provider's
//!   stored login is used as-is, **no refresh attempt** — a stale
//!   token simply fails into the fallback.
//!
//! Absence ≠ zero (invariant 3): every failure path degrades to the
//! stale cache or an empty catalogue — [`FetchedCatalog::context_window_of`]
//! answering `None` — never to a guessed window.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::{Value, json};

/// How long a fetched catalogue stays fresh: 24 hours. The codex CLI
/// caches its own models listing for 300 s, but that freshness buys a
/// client choosing a model right now; a dashboard ceiling only needs to
/// not be embarrassingly stale, and catalogues move on week scales
/// (new models arrive; a context window changes rarely). 24 h is fresh
/// enough for the CTX column and one request per day is quiet for the
/// APIs. A refresh that fails (an endpoint down at refresh time)
/// degrades to the stale cache and retries next cycle.
pub const CACHE_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// The per-request budget for one models fetch: long enough for a
/// multi-hundred-model listing over a slow link, short enough that a
/// hung endpoint cannot stall the refresh task.
pub const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The cache sources, as named in their cache files and keyed into
/// [`FetchedCatalogs`]: openrouter (the public listing), anthropic (the
/// listing both anthropic backends share), codex_sub (the codex
/// backend's own models endpoint).
pub const SOURCES: &[&str] = &["openrouter", "openai_api", "anthropic", "codex_sub"];

/// One provider's response parser: the listing shape's reader.
pub type Parse = fn(&Value, i64) -> anyhow::Result<FetchedCatalog>;

/// The parser for a cache source's listing shape, by source name.
pub fn parse_for(source: &str) -> Option<Parse> {
    match source {
        "openrouter" => Some(parse_openrouter),
        "openai_api" => Some(parse_openai),
        "anthropic" => Some(parse_anthropic),
        "codex_sub" => Some(parse_codex),
        _ => None,
    }
}

/// One model as a provider's listing named it.
#[derive(Debug, Clone, PartialEq)]
pub struct FetchedModel {
    /// The model id exactly as the provider listed it — openrouter's
    /// `id` (`z-ai/glm-5.3`), codex's `slug`, anthropic's `id`. This is
    /// the lookup key, byte-exact against what the ledger recorded.
    pub id: String,
    /// The model's context window in tokens, when the listing named
    /// one. `None` for any entry whose window field was absent or not
    /// a positive number (an anthropic presence-only entry included) —
    /// never a fabricated window.
    pub context_window: Option<u64>,
    /// The whole provider entry, verbatim: the "other interesting
    /// stuff" (openrouter's pricing and supported_parameters, codex's
    /// reasoning levels and truncation policy) preserved for later
    /// consumers. Kept as received; never re-serialised onto any wire.
    pub raw: Value,
}

/// One provider's fetched listing, parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct FetchedCatalog {
    /// When the listing was fetched, unix epoch milliseconds — the
    /// cache's age stamp, kept with the catalogue so a stale catalogue
    /// stays visibly stale in provenance, not just on disk.
    pub fetched_at_ms: i64,
    /// The listed models, in the provider's order.
    pub models: Vec<FetchedModel>,
}

impl FetchedCatalog {
    /// The model's context window from this listing — an exact id
    /// match only: the id the provider listed, byte-for-byte the id
    /// the ledger recorded. No alias folding, no family guessing: a
    /// dated snapshot never inherits its dateless model's window and a
    /// sibling never shares one (the exact-identity discipline,
    /// [`super::windows`]; an unmatched id stays `None`, never a
    /// guess — invariant 3).
    pub fn context_window_of(&self, model: &str) -> Option<u64> {
        self.models
            .iter()
            .find(|entry| entry.id == model)
            .and_then(|entry| entry.context_window)
    }

    /// The model's cache-write price from this listing, when it names
    /// one: the raw entry's `pricing.input_cache_write`, exact-id match
    /// like [`FetchedCatalog::context_window_of`]. Openrouter writes
    /// prices as strings (`"0.0000025"`); a number reads too. `None`
    /// covers every absence — no entry, no `pricing` object, no field —
    /// and the free signal is exactly one of those absences, so the
    /// caller that wants a verdict uses [`cache_writes_free`], which
    /// distinguishes "the entry exists and charges nothing" from
    /// "the listing says nothing at all".
    ///
    /// [`cache_writes_free`]: FetchedCatalog::cache_writes_free
    pub fn cache_write_price(&self, model: &str) -> Option<f64> {
        let entry = self.models.iter().find(|entry| entry.id == model)?;
        price_at(entry.raw.get("pricing")?, "input_cache_write")
    }

    /// Whether this listing says the model's cache writes cost nothing:
    /// `Some(true)` only on POSITIVE evidence — the entry exists, it
    /// carries a `pricing` object, and that object omits
    /// `input_cache_write` (openrouter's documented free signal: the
    /// docs say cache writes are free for such models, and the field is
    /// simply absent) or prices it at zero. `Some(false)` when a
    /// positive price is named. `None` when the model is unknown to the
    /// catalogue, its entry carries no `pricing` object to read at all
    /// (the anthropic and codex listings), or its write
    /// price is present but will not parse — absence is never a free
    /// verdict (invariant 3): unknown reads as charged, the
    /// conservative direction for a gate that fires.
    pub fn cache_writes_free(&self, model: &str) -> Option<bool> {
        let entry = self.models.iter().find(|entry| entry.id == model)?;
        let pricing = entry.raw.get("pricing")?;
        if !pricing.is_object() {
            return None;
        }
        match pricing.get("input_cache_write") {
            // The itemised object omits the write price: free.
            None => Some(true),
            // A price that will not parse is not a number — no verdict,
            // never free: `Some(false)` stays a POSITIVE charge, the
            // symmetric of the positive free verdict above.
            Some(value) => price_of(value).map(|price| price <= 0.0),
        }
    }
}

/// The live set of fetched catalogues, one per source — what the
/// daemon's background task swaps wholesale and any reader
/// ([`crate::catalog::windows::resolve_context_window`]'s callers)
/// consults per provider.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FetchedCatalogs {
    by_source: HashMap<&'static str, FetchedCatalog>,
}

impl FetchedCatalogs {
    /// Replace (or first install) one source's catalogue.
    pub fn set(&mut self, source: &'static str, catalog: FetchedCatalog) {
        self.by_source.insert(source, catalog);
    }

    /// One source's catalogue.
    pub fn get(&self, source: &str) -> Option<&FetchedCatalog> {
        self.by_source.get(source)
    }

    /// The cache source whose listing covers a backend by provider id:
    /// both anthropic backends share anthropic's listing,
    /// openrouter, openai_api and codex_sub are their own. A provider with no
    /// source has no fetched catalogue at all.
    pub fn source_of(provider: &str) -> Option<&'static str> {
        match provider {
            "openrouter" => Some("openrouter"),
            "openai_api" => Some("openai_api"),
            "codex_sub" => Some("codex_sub"),
            // One upstream listing covers both anthropic backends; it
            // carries windows but no prices.
            "anthropic_sub" | "anthropic_api" => Some("anthropic"),
            _ => None,
        }
    }

    /// The fetched context window for a model as served by
    /// `provider` — the ledger's `provider` column value, mapped onto
    /// the source whose listing covers that backend. A provider with no
    /// fetched catalogue answers `None` (no window, never a guess).
    pub fn context_window_of(&self, provider: &str, model: &str) -> Option<u64> {
        self.by_source
            .get(Self::source_of(provider)?)
            .and_then(|catalog| catalog.context_window_of(model))
    }

    /// [`FetchedCatalog::cache_writes_free`] for a model as served by
    /// `provider` — the same provider→source mapping
    /// [`FetchedCatalogs::context_window_of`] uses. `None` for a
    /// provider with no fetched catalogue or a model the listing does
    /// not carry: unknown is never free.
    pub fn cache_writes_free(&self, provider: &str, model: &str) -> Option<bool> {
        self.by_source
            .get(Self::source_of(provider)?)
            .and_then(|catalog| catalog.cache_writes_free(model))
    }
}

/// One provider's models endpoint: what to GET and how to authenticate.
/// The parser comes from [`parse_for`] on `provider`, so a source and
/// its listing shape are named in exactly one place.
pub struct CatalogSource {
    /// The cache-file name and [`FetchedCatalogs`] key — one of
    /// [`SOURCES`].
    pub provider: &'static str,
    /// The GET URL, query included (the codex `client_version` rides
    /// here, like the codex CLI's own request).
    pub url: reqwest::Url,
    /// The request headers: the credential when the endpoint needs one
    /// (codex's bearer, anthropic's key or a borrowed bearer), and
    /// anthropic's version header. Credential values are marked
    /// sensitive so a `Debug` of the map never prints them.
    pub headers: reqwest::header::HeaderMap,
    /// Whether this source may GET at all. `false` for a source toker
    /// holds no credential for (anthropic without an API key): the
    /// fetch would only 401, so refresh answers from the cache alone.
    pub fetch: bool,
}

/// The bearer `authorization` header for a source, marked sensitive.
/// `None` when the token is not a valid header value: the source goes
/// without it and 401s visibly, never a panic over a credential.
pub fn bearer_header(token: &str) -> Option<reqwest::header::HeaderValue> {
    let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).ok()?;
    value.set_sensitive(true);
    Some(value)
}

/// `$XDG_DATA_HOME/toker/models-cache`, falling back to
/// `~/.local/share/toker/models-cache` — the same resolution as the
/// ledger's [`crate::store::default_db_path`], so the caches live
/// beside the db the daemon already owns.
pub fn cache_dir() -> anyhow::Result<PathBuf> {
    let data_home = match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .context("no data home: set XDG_DATA_HOME or HOME")?;
            PathBuf::from(home).join(".local/share")
        }
    };
    Ok(data_home.join("toker").join("models-cache"))
}

/// One source's cache file: `<dir>/<provider>.json`.
pub fn cache_path(dir: &Path, provider: &str) -> PathBuf {
    dir.join(format!("{provider}.json"))
}

/// Load one source's cache: `(fetched_at_ms, the provider's raw
/// response)`. `None` when absent or unusable — a cache that cannot be
/// read or parsed is absent, never an error worth failing a refresh
/// over (the fetch that follows repairs it).
pub fn load_cache(dir: &Path, provider: &str) -> Option<(i64, Value)> {
    let raw = match std::fs::read_to_string(cache_path(dir, provider)) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::debug!(provider, %error, "models cache unreadable; treating it as absent");
            return None;
        }
    };
    let value: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
            tracing::debug!(provider, %error, "models cache does not parse; treating it as absent");
            return None;
        }
    };
    let fetched_at_ms = value
        .get("fetched_at_ms")
        .and_then(Value::as_i64)
        .with_context(|| format!("models cache for {provider} has no integer fetched_at_ms"))
        .ok()?;
    let response = value.get("response").cloned()?;
    Some((fetched_at_ms, response))
}

/// Every source's catalogue as its cache file holds it, for a reader
/// that must not fetch (`toker watch-context-window`). A source whose
/// cache is absent, unreadable or unparseable is simply missing: its
/// models resolve without a fetched window, never with a guessed one.
/// Staleness is not checked, because a stale listing is still the
/// provider's last word and the reader has no better one.
pub fn load_cached(dir: &Path) -> FetchedCatalogs {
    let mut catalogs = FetchedCatalogs::default();
    for source in SOURCES {
        let Some((fetched_at, response)) = load_cache(dir, source) else {
            continue;
        };
        if let Some(parse) = parse_for(source)
            && let Ok(catalog) = parse(&response, fetched_at)
        {
            catalogs.set(source, catalog);
        }
    }
    catalogs
}

/// Persist one fetched response as a source's cache file: the setup
/// wizard's atomic primitive (temp beside the target, fsync, re-parse,
/// compare, rename), 0644 on a fresh file — machine-readable cache
/// bytes, never a secret. The wrapper holds `fetched_at_ms` and the
/// provider's raw response; parsing happens at load.
fn persist(dir: &Path, provider: &str, fetched_at_ms: i64, response: &Value) -> anyhow::Result<()> {
    let value = json!({"fetched_at_ms": fetched_at_ms, "response": response});
    let mut bytes = serde_json::to_vec_pretty(&value).context("serialising the models cache")?;
    bytes.push(b'\n');
    crate::setup::atomic::atomic_write_bytes(
        &cache_path(dir, provider),
        &bytes,
        Some(0o644),
        |temp, written| {
            let reparsed: Value = serde_json::from_slice(written)
                .with_context(|| format!("re-parsing the written cache {}", temp.display()))?;
            if reparsed == value {
                Ok(())
            } else {
                bail!("the written cache does not round-trip to the intended value")
            }
        },
    )
}

/// Refresh one source's fetched catalogue — the whole cache/fetch
/// discipline in one place:
///
/// 1. a fresh cache (`now − fetched_at <` [`CACHE_TTL_MS`]) that parses
///    is returned with no request at all;
/// 2. otherwise the endpoint is GET'd once under
///    [`FETCH_TIMEOUT`], parsed, persisted (0644, atomic), and
///    returned — a listing that comes back with zero models is
///    treated as a fetch failure (an upstream incident, not a real
///    catalogue: none of the three sources is ever legitimately
///    empty), so a bad fetch cannot evict good data;
/// 3. on ANY fetch/parse failure — logged at debug — the stale cache
///    parses and returns (a stale ceiling beats no ceiling, and the
///    `Declared` verdict already says where it came from);
/// 4. with nothing cached, an empty catalogue returns: absence, never
///    a fabricated window.
///
/// `now_ms` is the caller's clock (unix ms), passed in so tests pin
/// the TTL exactly and no hidden clock lives here.
pub async fn refresh(
    source: &CatalogSource,
    dir: &Path,
    http: &reqwest::Client,
    now_ms: i64,
) -> anyhow::Result<FetchedCatalog> {
    let parse = parse_for(source.provider)
        .with_context(|| format!("no listing parser for provider {:?}", source.provider))?;
    let cached = load_cache(dir, source.provider);

    // Fresh and parses: no request at all. A fresh cache that does not
    // parse falls through to the fetch, which repairs it.
    if let Some((fetched_at, response)) = &cached
        && now_ms.saturating_sub(*fetched_at) < CACHE_TTL_MS
        && let Ok(catalog) = parse(response, *fetched_at)
    {
        return Ok(catalog);
    }

    // A source with no credential to send answers from the cache alone.
    if !source.fetch {
        return Ok(stale_or_empty(cached, parse, now_ms));
    }

    // Stale or absent: one GET, then parse and persist. Any failure —
    // network, status, body, shape, or a zero-model listing — degrades
    // below, never propagates.
    let fetched = match fetch_once(source, http).await {
        Ok(response) => match parse(&response, now_ms) {
            Ok(catalog) if !catalog.models.is_empty() => {
                if let Err(error) = persist(dir, source.provider, now_ms, &response) {
                    // The catalogue is good; only its disk copy is
                    // missing — the next cycle re-fetches.
                    tracing::debug!(
                        provider = source.provider,
                        %error,
                        "models cache persist failed (keeping the fetched catalogue in memory)"
                    );
                }
                Some(catalog)
            }
            Ok(_) => {
                tracing::debug!(
                    provider = source.provider,
                    "models listing came back empty; treating it as an upstream incident"
                );
                None
            }
            Err(error) => {
                tracing::debug!(provider = source.provider, %error, "models listing does not parse");
                None
            }
        },
        Err(error) => {
            tracing::debug!(provider = source.provider, %error, "models fetch failed");
            None
        }
    };
    if let Some(catalog) = fetched {
        return Ok(catalog);
    }
    Ok(stale_or_empty(cached, parse, now_ms))
}

/// The stale cache — however old — beats an empty answer; with nothing
/// cached that parses, an empty catalogue.
fn stale_or_empty(cached: Option<(i64, Value)>, parse: Parse, now_ms: i64) -> FetchedCatalog {
    if let Some((fetched_at, response)) = &cached
        && let Ok(catalog) = parse(response, *fetched_at)
    {
        return catalog;
    }
    FetchedCatalog {
        fetched_at_ms: now_ms,
        models: Vec::new(),
    }
}

/// One GET against a source's endpoint: the 10 s budget, the source's
/// headers, and any non-2xx status is the failure that refresh falls
/// back from.
async fn fetch_once(source: &CatalogSource, http: &reqwest::Client) -> anyhow::Result<Value> {
    let request = http
        .get(source.url.clone())
        .timeout(FETCH_TIMEOUT)
        .headers(source.headers.clone());
    let response = request
        .send()
        .await
        .with_context(|| format!("GET {}", source.url))?;
    let status = response.status();
    if !status.is_success() {
        bail!("GET {} → HTTP {status}", source.url);
    }
    response
        .json()
        .await
        .with_context(|| format!("parsing the models response from {}", source.url))
}

/// Parse openrouter's listing: `{data: [{id, context_length, pricing,
/// supported_parameters, …}]}` → id from `id`, window from
/// `context_length`.
///
/// A response without a `data` array is an error (the shape broke);
/// an entry without an `id`, or that is not an object, is skipped — a
/// malformed entry loses its own ceiling, never the catalogue.
pub fn parse_openrouter(response: &Value, fetched_at_ms: i64) -> anyhow::Result<FetchedCatalog> {
    let data = response
        .get("data")
        .and_then(Value::as_array)
        .context("openrouter models listing: no `data` array")?;
    let models = data
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(Value::as_str)?;
            Some(FetchedModel {
                id: id.to_owned(),
                context_window: window_at(entry, "context_length"),
                raw: entry.clone(),
            })
        })
        .collect();
    Ok(FetchedCatalog {
        fetched_at_ms,
        models,
    })
}

/// Parse OpenAI's models listing. It intentionally provides no context
/// ceiling: the endpoint documents identity and ownership, not token limits.
pub fn parse_openai(response: &Value, fetched_at_ms: i64) -> anyhow::Result<FetchedCatalog> {
    let data = response
        .get("data")
        .and_then(Value::as_array)
        .context("openai models listing: no `data` array")?;
    let models = data
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.trim();
            (!id.is_empty()).then(|| FetchedModel {
                id: id.to_owned(),
                context_window: None,
                raw: entry.clone(),
            })
        })
        .collect();
    Ok(FetchedCatalog {
        fetched_at_ms,
        models,
    })
}

/// Parse the codex backend's listing: `{"models": [{slug,
/// context_window, max_context_window, …}]}` → id from `slug`.
///
/// **The one window rule, documented**: the ceiling is
/// `max_context_window` when the listing names one, falling back to
/// `context_window`. This is the hand-verified catalogue's own codex
/// stance (`declared(default, max)` — "only the maximum is displayed,
/// as a declared provider ceiling"), extended to listings; the codex
/// CLI's `resolved_context_window` (context first) is its *session
/// default*, which is not what a dashboard ceiling shows.
pub fn parse_codex(response: &Value, fetched_at_ms: i64) -> anyhow::Result<FetchedCatalog> {
    let models = response
        .get("models")
        .and_then(Value::as_array)
        .context("codex models listing: no `models` array")?;
    let models = models
        .iter()
        .filter_map(|entry| {
            let id = entry.get("slug").and_then(Value::as_str)?;
            Some(FetchedModel {
                id: id.to_owned(),
                context_window: window_at(entry, "max_context_window")
                    .or_else(|| window_at(entry, "context_window")),
                raw: entry.clone(),
            })
        })
        .collect();
    Ok(FetchedCatalog {
        fetched_at_ms,
        models,
    })
}

/// Parse anthropic's `GET /v1/models` listing: `{data: [{type: "model",
/// id, display_name, created_at, max_input_tokens, max_tokens, …}]}`
/// (the documented List Models shape). `max_input_tokens` is the
/// window; an entry without a positive one is presence only, never a
/// guessed window. One page is parsed: the source asks for the API's
/// maximum page of 1000, far above anthropic's model count, so
/// `has_more` is not followed.
pub fn parse_anthropic(response: &Value, fetched_at_ms: i64) -> anyhow::Result<FetchedCatalog> {
    let data = response
        .get("data")
        .and_then(Value::as_array)
        .context("anthropic models listing: no `data` array")?;
    let models = data
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(Value::as_str)?;
            Some(FetchedModel {
                id: id.to_owned(),
                context_window: window_at(entry, "max_input_tokens"),
                raw: entry.clone(),
            })
        })
        .collect();
    Ok(FetchedCatalog {
        fetched_at_ms,
        models,
    })
}

/// A strictly-positive integer field, the window catalogue's own
/// semantics ([`super::windows`]): absent, fractional, negative, zero,
/// and string numbers are all absent — never a fabricated window.
fn window_at(entry: &Value, key: &str) -> Option<u64> {
    entry.get(key).and_then(Value::as_u64).filter(|&v| v > 0)
}

/// One price field of a `pricing` object, parsed: openrouter writes
/// prices as strings (`"0.0000025"`), a JSON number reads too, and
/// anything else is absent — a price that will not parse is never a
/// verdict.
fn price_of(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// [`price_of`] over `pricing.<key>`.
fn price_at(pricing: &Value, key: &str) -> Option<f64> {
    pricing.get(key).and_then(price_of)
}

#[cfg(test)]
mod tests {
    use super::{
        CACHE_TTL_MS, CatalogSource, FetchedCatalog, FetchedCatalogs, FetchedModel, SOURCES,
        cache_path, load_cache, parse_anthropic, parse_codex, parse_for, parse_openai,
        parse_openrouter, refresh,
    };
    use crate::setup::test_dir;
    use serde_json::{Value, json};

    /// A fixed frame time, unix ms — arbitrary but stable, like the
    /// TUI tests' NOW.
    const NOW: i64 = 2_000_000_000_000;

    // ── realistic fixture listings (hand-written from the shapes) ──

    /// Openrouter's shape: `{data: [{id, context_length, pricing,
    /// supported_parameters, …}]}` — three entries: a full one, a
    /// sibling of the first (the exact-match discipline's foil), and
    /// one with no `context_length` at all.
    fn openrouter_listing() -> Value {
        json!({
            "data": [
                {
                    "id": "z-ai/glm-5.3",
                    "name": "GLM 5.3",
                    "created": 1_760_000_000,
                    "description": "Z.ai GLM 5.3",
                    "context_length": 200_000,
                    "architecture": {
                        "modality": "text->text",
                        "input_modalities": ["text", "image"],
                        "output_modalities": ["text"],
                        "tokenizer": "Other",
                        "instruct_type": null
                    },
                    "pricing": {
                        "prompt": "0.00000011",
                        "completion": "0.00000043",
                        "request": "0",
                        "image": "0",
                        "web_search": "0",
                        "internal_reasoning": "0",
                        "input_cache_read": "0.0000000022",
                        "input_cache_write": "0"
                    },
                    "top_provider": {
                        "context_length": 200_000,
                        "max_completion_tokens": 128_000
                    },
                    "per_request_limits": null,
                    "supported_parameters": [
                        "tools", "tool_choice", "max_tokens", "reasoning", "stream", "stop"
                    ]
                },
                {
                    "id": "z-ai/glm-5.3-flash",
                    "name": "GLM 5.3 Flash",
                    "context_length": 131_072,
                    "pricing": {"prompt": "0.000000015", "completion": "0.00000006"},
                    "supported_parameters": ["tools", "max_tokens"]
                },
                {
                    "id": "windowless/model",
                    "name": "A listing entry with no context_length at all",
                    "pricing": {"prompt": "0"}
                }
            ]
        })
    }

    /// Codex's shape (the vendored models.json's): `{"models": [{slug,
    /// context_window, max_context_window, supported_reasoning_levels,
    /// truncation_policy, …}]}` — both windows, only the default, only
    /// the max.
    fn codex_listing() -> Value {
        json!({
            "models": [
                {
                    "slug": "gpt-5.6-sol",
                    "display_name": "GPT-5.6 Sol",
                    "description": "Balanced work",
                    "supported_reasoning_levels": [
                        {"effort": "low", "description": "Fast"},
                        {"effort": "medium", "description": "Balanced"},
                        {"effort": "high", "description": "Deep"}
                    ],
                    "truncation_policy": {"mode": "tokens", "limit": 10_000},
                    "supports_parallel_tool_calls": true,
                    "tool_mode": "code_mode",
                    "context_window": 272_000,
                    "max_context_window": 872_000,
                    "auto_compact_token_limit": null
                },
                {
                    "slug": "gpt-6-terra",
                    "display_name": "GPT-6 Terra",
                    "supported_reasoning_levels": [{"effort": "low", "description": "Fast"}],
                    "truncation_policy": {"mode": "tokens", "limit": 10_000},
                    "context_window": 200_000
                },
                {
                    "slug": "legacy-max-only",
                    "display_name": "Legacy",
                    "max_context_window": 128_000
                }
            ]
        })
    }

    /// Anthropic's documented List Models shape: `{data: [{type:
    /// "model", id, display_name, created_at, max_input_tokens,
    /// max_tokens}], has_more, first_id, last_id}` (capabilities
    /// elided). The second entry is the listing's older shape, with no
    /// window field at all.
    fn anthropic_listing() -> Value {
        json!({
            "data": [
                {
                    "type": "model",
                    "id": "claude-opus-4-5-20251101",
                    "display_name": "Claude Opus 4.5 (new)",
                    "created_at": "2025-11-01T00:00:00Z",
                    "max_input_tokens": 200_000,
                    "max_tokens": 64_000
                },
                {
                    "type": "model",
                    "id": "claude-sonnet-4-5-20250929",
                    "display_name": "Claude Sonnet 4.5 (new)",
                    "created_at": "2025-09-29T00:00:00Z"
                }
            ],
            "has_more": false,
            "first_id": "claude-opus-4-5-20251101",
            "last_id": "claude-sonnet-4-5-20250929"
        })
    }

    #[test]
    fn openrouter_parses_ids_windows_and_preserves_raw() {
        let catalog = parse_openrouter(&openrouter_listing(), NOW).expect("parse");
        assert_eq!(catalog.fetched_at_ms, NOW);
        let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["z-ai/glm-5.3", "z-ai/glm-5.3-flash", "windowless/model"]
        );
        assert_eq!(catalog.models[0].context_window, Some(200_000));
        assert_eq!(catalog.models[1].context_window, Some(131_072));
        assert_eq!(
            catalog.models[2].context_window, None,
            "a missing context_length is absent, never zero"
        );
        // The "other interesting stuff" rides verbatim.
        assert_eq!(
            catalog.models[0].raw["pricing"]["prompt"], "0.00000011",
            "pricing kept as received"
        );
        assert_eq!(
            catalog.models[0].raw["supported_parameters"][0], "tools",
            "supported_parameters kept as received"
        );
        assert_eq!(
            catalog.models[1].raw["architecture"]["modality"],
            Value::Null,
            "an entry the fixture did not carry reads null, not an invented field"
        );
    }

    #[test]
    fn codex_takes_the_slug_and_the_maximum_as_the_ceiling() {
        let catalog = parse_codex(&codex_listing(), NOW).expect("parse");
        let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["gpt-5.6-sol", "gpt-6-terra", "legacy-max-only"]);
        // The one rule: max_context_window when present, else the
        // default — the hand-verified catalogue's own codex stance.
        assert_eq!(
            catalog.models[0].context_window,
            Some(872_000),
            "the maximum is the ceiling, matching how the hand-verified catalogue displays gpt-5.6-sol"
        );
        assert_eq!(
            catalog.models[1].context_window,
            Some(200_000),
            "a listing with only a default falls to it"
        );
        assert_eq!(
            catalog.models[2].context_window,
            Some(128_000),
            "a listing with only a maximum takes it"
        );
        // Raw preserved: the reasoning levels and truncation policy a
        // later consumer would want.
        assert_eq!(
            catalog.models[0].raw["supported_reasoning_levels"][2]["effort"],
            "high"
        );
        assert_eq!(catalog.models[0].raw["truncation_policy"]["mode"], "tokens");
        assert_eq!(catalog.models[0].raw["truncation_policy"]["limit"], 10_000);
    }

    #[test]
    fn anthropic_takes_max_input_tokens_as_the_window() {
        let catalog = parse_anthropic(&anthropic_listing(), NOW).expect("parse");
        let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["claude-opus-4-5-20251101", "claude-sonnet-4-5-20250929"]
        );
        assert_eq!(
            catalog.context_window_of("claude-opus-4-5-20251101"),
            Some(200_000),
            "max_input_tokens is the window, not max_tokens"
        );
        assert_eq!(
            catalog.context_window_of("claude-sonnet-4-5-20250929"),
            None,
            "an entry in the older shape is presence only"
        );
        assert_eq!(
            catalog.models[0].raw["display_name"], "Claude Opus 4.5 (new)",
            "raw kept for a later consumer"
        );
        // An unlisted id answers nothing.
        assert_eq!(catalog.context_window_of("claude-opus-5"), None);

        // The documented example answers `0` for both limits, and the
        // field is nullable: neither is a window.
        let listing = json!({"data": [
            {"id": "zero", "max_input_tokens": 0},
            {"id": "null", "max_input_tokens": null},
            {"id": "str", "max_input_tokens": "1000000"}
        ]});
        let catalog = parse_anthropic(&listing, NOW).expect("parse");
        assert_eq!(catalog.models.len(), 3, "presence entries still parse");
        for model in &catalog.models {
            assert_eq!(model.context_window, None, "{} has no window", model.id);
        }
    }

    /// A pricing-shaped openrouter listing for the cache-write rule:
    /// charged by string (the live gpt-5.6-sol figure), free by absence
    /// (the live z-ai shape — the docs say writes are free and the
    /// field is simply not there), free by zero, an entry with no
    /// pricing object at all, and a non-parseable price.
    fn pricing_listing() -> Value {
        json!({
            "data": [
                {
                    "id": "openai/gpt-5.6-sol",
                    "context_length": 872_000,
                    "pricing": {
                        "prompt": "0.00000125",
                        "completion": "0.00001",
                        "input_cache_read": "0.000000125",
                        "input_cache_write": "0.0000025"
                    }
                },
                {
                    "id": "z-ai/glm-5.3",
                    "context_length": 200_000,
                    "pricing": {
                        "prompt": "0.00000011",
                        "completion": "0.00000043",
                        "input_cache_read": "0.0000000022"
                    }
                },
                {
                    "id": "zero-write/model",
                    "context_length": 128_000,
                    "pricing": {
                        "prompt": "0.0000002",
                        "input_cache_write": "0"
                    }
                },
                {
                    "id": "zero-write-number/model",
                    "context_length": 128_000,
                    "pricing": {
                        "prompt": "0.0000002",
                        "input_cache_write": 0
                    }
                },
                {
                    "id": "unpriced/model",
                    "context_length": 64_000
                },
                {
                    "id": "garbage-write/model",
                    "context_length": 64_000,
                    "pricing": {
                        "prompt": "0.0000002",
                        "input_cache_write": "free, trust me"
                    }
                },
                {
                    "id": "presence/only",
                    "display_name": "A presence-list entry, no pricing"
                }
            ]
        })
    }

    #[test]
    fn cache_write_pricing_reads_the_pricing_object_with_absence_as_free() {
        let catalog = parse_openrouter(&pricing_listing(), NOW).expect("parse");

        // Charged: a positive price, string or number, is Some(false).
        assert_eq!(
            catalog.cache_write_price("openai/gpt-5.6-sol"),
            Some(0.0000025),
            "the live openrouter figure, parsed from its string form"
        );
        assert_eq!(catalog.cache_writes_free("openai/gpt-5.6-sol"), Some(false));

        // Free by absence: the entry exists, itemises prices, and omits
        // the write field — the documented z-ai signal.
        assert_eq!(
            catalog.cache_writes_free("z-ai/glm-5.3"),
            Some(true),
            "absent IS the free signal for an itemised pricing object"
        );
        assert_eq!(
            catalog.cache_write_price("z-ai/glm-5.3"),
            None,
            "the raw price lookup cannot distinguish free-by-absence from unknown"
        );

        // Free by zero, both wire forms.
        assert_eq!(catalog.cache_writes_free("zero-write/model"), Some(true));
        assert_eq!(catalog.cache_write_price("zero-write/model"), Some(0.0));
        assert_eq!(
            catalog.cache_writes_free("zero-write-number/model"),
            Some(true)
        );

        // An entry with no pricing object says nothing about prices:
        // unknown, never free — the anthropic listing is exactly this
        // shape, and anthropic does charge for cache writes.
        assert_eq!(catalog.cache_writes_free("unpriced/model"), None);
        assert_eq!(catalog.cache_writes_free("presence/only"), None);

        // A price that will not parse is not a number, and never free.
        assert_eq!(catalog.cache_writes_free("garbage-write/model"), None);
        assert_eq!(catalog.cache_write_price("garbage-write/model"), None);

        // Unknown to the catalogue: never treated as free.
        assert_eq!(catalog.cache_writes_free("no/such-model"), None);
        assert_eq!(catalog.cache_write_price("no/such-model"), None);
        // Byte-exact, like every listing lookup.
        assert_eq!(catalog.cache_writes_free("Z-AI/GLM-5.3"), None);
    }

    #[test]
    fn cache_write_pricing_maps_providers_onto_sources() {
        let mut catalogs = FetchedCatalogs::default();
        catalogs.set(
            "openrouter",
            parse_openrouter(&pricing_listing(), NOW).expect("parse"),
        );
        catalogs.set(
            "anthropic",
            parse_anthropic(&anthropic_listing(), NOW).expect("parse"),
        );

        // The openrouter backend consults its own listing.
        assert_eq!(
            catalogs.cache_writes_free("openrouter", "z-ai/glm-5.3"),
            Some(true)
        );
        assert_eq!(
            catalogs.cache_writes_free("openrouter", "openai/gpt-5.6-sol"),
            Some(false)
        );
        // Both anthropic backends share one listing, which carries no
        // pricing object: unknown, never free — so the anthropic cold
        // gate keeps firing for claude models against the real listing.
        for provider in ["anthropic_sub", "anthropic_api"] {
            assert_eq!(
                catalogs.cache_writes_free(provider, "claude-opus-4-5-20251101"),
                None,
                "an anthropic entry has no prices to read"
            );
        }
        // A provider with no catalogue, and per-source isolation: a
        // codex answer never comes from the openrouter listing.
        assert_eq!(
            catalogs.cache_writes_free("codex_sub", "z-ai/glm-5.3"),
            None
        );
        assert_eq!(
            catalogs.cache_writes_free("lunaroute", "z-ai/glm-5.3"),
            None
        );
    }

    #[test]
    fn window_lookup_is_exact_only_no_aliases_no_families() {
        let catalog = parse_openrouter(&openrouter_listing(), NOW).expect("parse");
        assert_eq!(catalog.context_window_of("z-ai/glm-5.3"), Some(200_000));
        // A sibling never inherits (no family guessing).
        assert_eq!(
            catalog.context_window_of("z-ai/glm-5.3-flash"),
            Some(131_072)
        );
        assert_eq!(catalog.context_window_of("z-ai/glm-5.3-pro"), None);
        // Byte-exact: the ledger records the provider's own spelling.
        assert_eq!(catalog.context_window_of("Z-AI/GLM-5.3"), None);
        // An entry with no window answers None, never a guess.
        assert_eq!(catalog.context_window_of("windowless/model"), None);
        // A dated snapshot of a listed claude-named model must not
        // inherit the dateless entry's window (the alias discipline).
        let mut listing = anthropic_listing();
        listing["data"]
            .as_array_mut()
            .expect("data array")
            .push(json!({"id": "anthropic/claude-opus-4.5", "context_length": 200_000}));
        let anthropic_windowed = parse_openrouter(&listing, NOW).expect("parse");
        assert_eq!(
            anthropic_windowed.context_window_of("anthropic/claude-opus-4.5-20251101"),
            None,
            "a dated snapshot is not its dateless model"
        );
        assert_eq!(
            anthropic_windowed.context_window_of("anthropic/claude-opus-4.5"),
            Some(200_000)
        );
    }

    #[test]
    fn fetched_catalogs_map_provider_ids_onto_sources() {
        let mut catalogs = FetchedCatalogs::default();
        catalogs.set(
            "openrouter",
            FetchedCatalog {
                fetched_at_ms: NOW,
                models: vec![FetchedModel {
                    id: "z-ai/glm-5.3".to_owned(),
                    context_window: Some(200_000),
                    raw: json!({"id": "z-ai/glm-5.3"}),
                }],
            },
        );
        catalogs.set(
            "codex_sub",
            FetchedCatalog {
                fetched_at_ms: NOW,
                models: vec![FetchedModel {
                    id: "gpt-6-terra".to_owned(),
                    context_window: Some(200_000),
                    raw: json!({"slug": "gpt-6-terra"}),
                }],
            },
        );
        catalogs.set(
            "anthropic",
            FetchedCatalog {
                fetched_at_ms: NOW,
                models: vec![FetchedModel {
                    id: "claude-opus-4-5-20251101".to_owned(),
                    context_window: Some(200_000),
                    raw: json!({"id": "claude-opus-4-5-20251101"}),
                }],
            },
        );

        assert_eq!(
            catalogs.context_window_of("openrouter", "z-ai/glm-5.3"),
            Some(200_000)
        );
        assert_eq!(
            catalogs.context_window_of("codex_sub", "gpt-6-terra"),
            Some(200_000)
        );
        // Both anthropic backends share the one anthropic listing.
        for provider in ["anthropic_sub", "anthropic_api"] {
            assert_eq!(
                catalogs.context_window_of(provider, "claude-opus-4-5-20251101"),
                Some(200_000),
                "{provider} reads the shared anthropic listing"
            );
        }
        // A provider with no catalogue (unwired, or a serving-provider
        // label) answers nothing.
        assert_eq!(
            catalogs.context_window_of("lunaroute", "z-ai/glm-5.3"),
            None
        );
        // An openrouter row does not consult the codex listing and vice
        // versa: per-provider isolation.
        assert_eq!(
            catalogs.context_window_of("openrouter", "gpt-6-terra"),
            None
        );
        assert_eq!(
            catalogs.context_window_of("codex_sub", "z-ai/glm-5.3"),
            None
        );
        // The cache-file names are exactly the four sources.
        assert_eq!(
            SOURCES,
            &["openrouter", "openai_api", "anthropic", "codex_sub"]
        );
        for source in SOURCES {
            assert!(parse_for(source).is_some(), "{source} has a parser");
        }
        assert!(parse_for("nope").is_none());
    }

    #[test]
    fn broken_responses_error_and_broken_entries_skip() {
        // Whole-shape failures: errors, never empty catalogues.
        for bad in [
            json!({}),
            json!({"data": {}}),
            json!({"data": 42}),
            json!("a string"),
        ] {
            assert!(
                parse_openrouter(&bad, NOW).is_err(),
                "openrouter shape broke: {bad}"
            );
            assert!(
                parse_openai(&bad, NOW).is_err(),
                "openai shape broke: {bad}"
            );
            assert!(
                parse_anthropic(&bad, NOW).is_err(),
                "anthropic shape broke: {bad}"
            );
        }

        let openai = parse_openai(
            &json!({
                "data": [
                    {"id": "gpt-6.1-sol", "created": 1, "owned_by": "openai"},
                    {"id": ""},
                    {"owned_by": "openai"}
                ]
            }),
            NOW,
        )
        .expect("openai listing");
        assert_eq!(openai.models.len(), 1);
        assert_eq!(openai.models[0].id, "gpt-6.1-sol");
        assert_eq!(openai.models[0].context_window, None);
        assert_eq!(openai.models[0].raw["owned_by"], "openai");
        for bad in [json!({}), json!({"models": "no"}), json!([])] {
            assert!(parse_codex(&bad, NOW).is_err(), "codex shape broke: {bad}");
        }

        // Entry-level failures: skipped, never fatal — a malformed
        // entry loses its own ceiling, not the catalogue.
        let listing = json!({
            "data": [
                "not an object",
                {"context_length": 5},
                {"id": "fine/model", "context_length": 7_000}
            ]
        });
        let catalog = parse_openrouter(&listing, NOW).expect("parse");
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].id, "fine/model");
        assert_eq!(catalog.models[0].context_window, Some(7_000));

        let listing = json!({"models": [null, {"context_window": 5}, {"slug": "ok-slug"}]});
        let catalog = parse_codex(&listing, NOW).expect("parse");
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].context_window, None);

        let listing = json!({"data": [{"display_name": "no id"}, {"id": "ok-id"}]});
        let catalog = parse_anthropic(&listing, NOW).expect("parse");
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].id, "ok-id");

        // Non-positive windows are absent, never zero.
        let listing = json!({"data": [
            {"id": "zero", "context_length": 0},
            {"id": "neg", "context_length": -1},
            {"id": "frac", "context_length": 1.5},
            {"id": "str", "context_length": "200000"}
        ]});
        let catalog = parse_openrouter(&listing, NOW).expect("parse");
        for model in &catalog.models {
            assert_eq!(model.context_window, None, "{} stays windowless", model.id);
        }
    }

    #[test]
    fn persistence_round_trips_the_raw_response_at_0644() {
        let dir = test_dir("round-trip");
        let response = openrouter_listing();
        super::persist(&dir, "openrouter", NOW, &response).expect("persist");

        // The file itself: 0644, provider-shaped (the raw response, not
        // a parsed catalogue).
        let mode = std::fs::metadata(cache_path(&dir, "openrouter"))
            .expect("cache file")
            .permissions();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            mode.mode() & 0o777,
            0o644,
            "a machine-readable cache, never 0600-only"
        );
        let (fetched_at, loaded) = load_cache(&dir, "openrouter").expect("load");
        assert_eq!(fetched_at, NOW);
        assert_eq!(loaded, response, "the provider's raw response, verbatim");

        // And the parse of the loaded cache is the same catalogue.
        let first = parse_openrouter(&response, NOW).expect("parse");
        let second = parse_openrouter(&loaded, fetched_at).expect("reparse");
        assert_eq!(first, second);

        // An unusable cache reads as absent: corrupt JSON, a missing
        // stamp, a non-integer stamp.
        std::fs::write(cache_path(&dir, "openrouter"), "{not json").expect("write");
        assert_eq!(load_cache(&dir, "openrouter"), None);
        std::fs::write(cache_path(&dir, "openrouter"), r#"{"response": {}}"#).expect("write");
        assert_eq!(load_cache(&dir, "openrouter"), None);
        std::fs::write(
            cache_path(&dir, "openrouter"),
            r#"{"fetched_at_ms": "not a number", "response": {}}"#,
        )
        .expect("write");
        assert_eq!(load_cache(&dir, "openrouter"), None);
        assert_eq!(
            load_cache(&dir, "no-such-provider"),
            None,
            "a missing file is the normal first-run state"
        );
    }

    // ── the fetch integration: an in-process axum mock ────────────

    use axum::Router;
    use axum::extract::{Request, State};
    use axum::http::{HeaderValue, StatusCode, header};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use std::sync::{Arc, Mutex};

    /// One recorded models request: method, full URI (query included),
    /// and the authorization header, when one was sent.
    #[derive(Debug, Clone, PartialEq)]
    struct Recorded {
        method: String,
        uri: String,
        authorization: Option<String>,
    }

    /// An in-process models endpoint: records every request, answers
    /// `status` with `body`. Any path answers (the sources' paths
    /// differ: `/v1/models`, `/models?client_version=…`).
    async fn spawn_models_mock(
        status: StatusCode,
        body: Value,
    ) -> (Arc<Mutex<Vec<Recorded>>>, reqwest::Url) {
        #[derive(Clone)]
        struct Mock {
            seen: Arc<Mutex<Vec<Recorded>>>,
            status: StatusCode,
            body: String,
        }

        async fn models(State(mock): State<Mock>, request: Request) -> Response {
            mock.seen.lock().unwrap().push(Recorded {
                method: request.method().to_string(),
                uri: request.uri().to_string(),
                authorization: request
                    .headers()
                    .get(header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
            });
            (
                mock.status,
                [(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                )],
                mock.body.clone(),
            )
                .into_response()
        }

        let mock = Mock {
            seen: Arc::new(Mutex::new(Vec::new())),
            status,
            body: body.to_string(),
        };
        let seen = mock.seen.clone();
        let app = Router::new()
            .route("/{*path}", get(models))
            .with_state(mock);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("mock binds");
        let addr = listener.local_addr().expect("mock addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("mock serves") });
        let url: reqwest::Url = format!("http://{addr}").parse().expect("mock url");
        (seen, url)
    }

    fn openrouter_source(url: reqwest::Url) -> CatalogSource {
        // String construction, not `Url::join` — the openrouter
        // provider's own rule (a base not ending in `/` drops its
        // last segment under RFC 3986 join).
        CatalogSource {
            provider: "openrouter",
            url: format!("{url}v1/models").parse().expect("source url"),
            headers: Default::default(),
            fetch: true,
        }
    }

    fn seen_of(seen: &Arc<Mutex<Vec<Recorded>>>) -> Vec<Recorded> {
        seen.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn a_fresh_cache_answers_without_a_request() {
        let dir = test_dir("fresh");
        super::persist(&dir, "openrouter", NOW - 1_000, &openrouter_listing())
            .expect("seed the cache");

        // A mock that would answer — and records the hit if it got one.
        let (seen, url) = spawn_models_mock(StatusCode::OK, json!({"data": []})).await;
        let http = reqwest::Client::new();
        let catalog = refresh(&openrouter_source(url), &dir, &http, NOW)
            .await
            .expect("refresh");
        assert!(
            seen_of(&seen).is_empty(),
            "a cache fetched a second ago must not hit the endpoint"
        );
        assert_eq!(
            catalog.models.len(),
            3,
            "the cached catalogue answers, at its own fetched_at"
        );
        assert_eq!(catalog.fetched_at_ms, NOW - 1_000);
    }

    #[tokio::test]
    async fn a_stale_cache_is_refetched_parsed_and_persisted() {
        let dir = test_dir("stale");
        // A day-and-a-second-old cache holding an OUTDATED listing: the
        // refresh must replace it.
        let mut stale = openrouter_listing();
        stale["data"]
            .as_array_mut()
            .expect("data")
            .push(json!({"id": "old/model", "context_length": 1_000}));
        super::persist(&dir, "openrouter", NOW - CACHE_TTL_MS, &stale).expect("seed");

        let (seen, url) = spawn_models_mock(StatusCode::OK, openrouter_listing()).await;
        let http = reqwest::Client::new();
        let catalog = refresh(&openrouter_source(url), &dir, &http, NOW)
            .await
            .expect("refresh");

        // Exactly one GET, at the openrouter path, unauthenticated.
        assert_eq!(
            seen_of(&seen),
            vec![Recorded {
                method: "GET".to_owned(),
                uri: "/v1/models".to_owned(),
                authorization: None,
            }]
        );
        assert_eq!(
            catalog.fetched_at_ms, NOW,
            "the fetched catalogue is stamped now"
        );
        assert!(
            !catalog.models.iter().any(|model| model.id == "old/model"),
            "the stale listing's model did not survive the fetch"
        );
        assert_eq!(
            catalog.context_window_of("z-ai/glm-5.3"),
            Some(200_000),
            "the fresh listing parsed"
        );
        // The cache on disk is the fetched one, fresh.
        let (fetched_at, response) = load_cache(&dir, "openrouter").expect("persisted");
        assert_eq!(fetched_at, NOW);
        assert_eq!(response, openrouter_listing());
    }

    #[tokio::test]
    async fn a_failed_fetch_falls_back_to_the_stale_cache() {
        let dir = test_dir("failed-cache");
        super::persist(
            &dir,
            "openrouter",
            NOW - CACHE_TTL_MS * 7,
            &openrouter_listing(),
        )
        .expect("seed a week-old cache");

        let (seen, url) =
            spawn_models_mock(StatusCode::INTERNAL_SERVER_ERROR, json!({"x": 1})).await;
        let http = reqwest::Client::new();
        let catalog = refresh(&openrouter_source(url), &dir, &http, NOW)
            .await
            .expect("refresh still answers");
        assert_eq!(seen_of(&seen).len(), 1, "one attempt, one fallback");
        assert_eq!(
            catalog.models.len(),
            3,
            "the stale catalogue beats no catalogue"
        );
        assert_eq!(
            catalog.fetched_at_ms,
            NOW - CACHE_TTL_MS * 7,
            "and stays visibly stale — its own fetched_at, not a lie"
        );
        // The cache file was NOT clobbered by the failure.
        let (fetched_at, _) = load_cache(&dir, "openrouter").expect("cache survives");
        assert_eq!(fetched_at, NOW - CACHE_TTL_MS * 7);
    }

    #[tokio::test]
    async fn a_failed_fetch_without_a_cache_is_an_empty_catalogue() {
        let dir = test_dir("failed-empty");
        let (seen, url) =
            spawn_models_mock(StatusCode::UNAUTHORIZED, json!({"error": "auth"})).await;
        let http = reqwest::Client::new();
        let catalog = refresh(&openrouter_source(url), &dir, &http, NOW)
            .await
            .expect("refresh still answers");
        assert_eq!(seen_of(&seen).len(), 1);
        assert_eq!(catalog.models.len(), 0, "absence, never fabricated windows");
        assert_eq!(catalog.context_window_of("z-ai/glm-5.3"), None);
        // The uncredentialed anthropic case is exactly this shape: a
        // 401 with no cache behind it reads empty, the hand-verified
        // catalogue covers claude, and the next cycle retries.
        assert!(
            !cache_path(&dir, "openrouter").exists(),
            "a failed fetch persists nothing"
        );
    }

    #[tokio::test]
    async fn an_empty_listing_is_treated_as_an_upstream_incident() {
        let dir = test_dir("incident");
        super::persist(
            &dir,
            "openrouter",
            NOW - CACHE_TTL_MS,
            &openrouter_listing(),
        )
        .expect("seed");

        let (seen, url) = spawn_models_mock(StatusCode::OK, json!({"data": []})).await;
        let http = reqwest::Client::new();
        let catalog = refresh(&openrouter_source(url), &dir, &http, NOW)
            .await
            .expect("refresh still answers");
        assert_eq!(seen_of(&seen).len(), 1, "the fetch happened");
        assert_eq!(
            catalog.models.len(),
            3,
            "a zero-model listing does not evict good data"
        );
        let (fetched_at, _) = load_cache(&dir, "openrouter").expect("cache survives");
        assert_eq!(
            fetched_at,
            NOW - CACHE_TTL_MS,
            "the incident was not persisted as a fresh empty catalogue"
        );
    }

    /// A source with nothing to authenticate with never GETs: a stale
    /// cache answers as it is, and no cache is an empty catalogue.
    #[tokio::test]
    async fn a_cache_only_source_answers_from_the_cache_without_a_request() {
        let dir = test_dir("cache-only");
        let (seen, url) = spawn_models_mock(StatusCode::OK, anthropic_listing()).await;
        let source = CatalogSource {
            provider: "anthropic",
            url: format!("{url}v1/models").parse().expect("source url"),
            headers: Default::default(),
            fetch: false,
        };
        let http = reqwest::Client::new();

        let empty = refresh(&source, &dir, &http, NOW).await.expect("refresh");
        assert!(empty.models.is_empty(), "nothing cached: absence");

        super::persist(
            &dir,
            "anthropic",
            NOW - 2 * CACHE_TTL_MS,
            &anthropic_listing(),
        )
        .expect("seed");
        let stale = refresh(&source, &dir, &http, NOW).await.expect("refresh");
        assert_eq!(stale.fetched_at_ms, NOW - 2 * CACHE_TTL_MS);
        assert_eq!(
            stale.context_window_of("claude-opus-4-5-20251101"),
            Some(200_000)
        );
        assert!(seen_of(&seen).is_empty(), "a cache-only source never GETs");
    }

    #[tokio::test]
    async fn the_codex_fetch_carries_the_bearer_and_the_client_version_query() {
        let dir = test_dir("codex-fetch");
        let (seen, url) = spawn_models_mock(StatusCode::OK, codex_listing()).await;
        let source = CatalogSource {
            provider: "codex_sub",
            url: format!("{url}models?client_version=0.154.0")
                .parse()
                .expect("source url"),
            headers: [(
                axum::http::header::AUTHORIZATION,
                super::bearer_header("codex-test-bearer").expect("valid bearer"),
            )]
            .into_iter()
            .collect(),
            fetch: true,
        };
        let http = reqwest::Client::new();
        let catalog = refresh(&source, &dir, &http, NOW).await.expect("refresh");
        assert_eq!(
            seen_of(&seen),
            vec![Recorded {
                method: "GET".to_owned(),
                uri: "/models?client_version=0.154.0".to_owned(),
                authorization: Some("Bearer codex-test-bearer".to_owned()),
            }],
            "the codex models GET: the query the CLI sends, the stored bearer, nothing else"
        );
        assert_eq!(catalog.context_window_of("gpt-5.6-sol"), Some(872_000));
        assert!(
            cache_path(&dir, "codex_sub").exists(),
            "persisted under the source name"
        );
    }

    /// The REAL openrouter listing, fetched once — the honest check of
    /// the CTX `?` fix: how many models carry a `context_length`, and
    /// what the catalogue yields for the models this machine actually
    /// uses (which the hand-verified catalogue does not carry, so they
    /// render `?` today). Public endpoint, no credentials; the only
    /// real network call in the suite, gated for deliberate runs.
    #[tokio::test]
    #[ignore = "calls the REAL openrouter models endpoint once (public, \
                no credentials); run deliberately with --ignored"]
    async fn the_real_openrouter_listing_yields_ceilings_for_this_machines_models() {
        let dir = test_dir("real-probe");
        let source = CatalogSource {
            provider: "openrouter",
            url: "https://openrouter.ai/api/v1/models"
                .parse()
                .expect("openrouter models url"),
            headers: Default::default(),
            fetch: true,
        };
        let http = reqwest::Client::new();
        let catalog = refresh(&source, &dir, &http, NOW)
            .await
            .expect("the real listing");

        let total = catalog.models.len();
        let with_window = catalog
            .models
            .iter()
            .filter(|model| model.context_window.is_some())
            .count();
        eprintln!("== the real openrouter listing ==");
        eprintln!(
            "{total} models listed; {with_window} carry a context_length ({} do not)",
            total - with_window
        );
        for model in [
            "z-ai/glm-5.3",
            "z-ai/glm-5.3-flash",
            "inclusionai/ling-3.1-flash",
            "openai/gpt-6-luna",
        ] {
            let fetched = catalog.context_window_of(model);
            let before =
                crate::catalog::windows::resolve_context_window(model, None, None, None, None);
            let after =
                crate::catalog::windows::resolve_context_window(model, None, fetched, None, None);
            eprintln!("  {model}: context_length {fetched:?} → CTX {before:?} becomes {after:?}");
        }
        assert!(total > 0, "the listing answered with models");
    }
}
