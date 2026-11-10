//! State tables: small, read-modify-write data
//! alongside the insert-only ledger — lanes, learned models, allowances,
//! pings, the last meter snapshot, and the meta key/value table.
//!
//! These are *not* part of the ledger's insert-only discipline: the store
//! exposes upserts and the caller owns the read-modify-write cycle (e.g.
//! bumping a lane's `updated_ms` and `prompt_tokens` after a response).
//! Expiry rules that depend on clocks (allowances self-expiring once their
//! window's reset passes, the 30-day lane prune) are the caller's too — the
//! store has no clock beyond `ts_ms` values handed to it.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use crate::ir::Release;
use crate::policy::{
    ActivationScope, BindingId, CandidateId, CanonicalLimits, FrontendScope, PolicyFingerprint,
    PresentationId, RecordedFacts, RewriteId, RouteFingerprint, RouteId, StableId,
    ValidatedPolicyDocument, decode_policy, policy_bytes, policy_fingerprint,
};
use serde_json::Value;

use super::{Error, Result, row_of, rows_of};
use rusqlite::{Connection, TransactionBehavior};

/// One lane row: `key` is the composite `"sessionId|toolsHash"`. `ping` lanes
/// never hold the sleep lock.
///
/// The predecessor's lane record, in store form: `updated_ms` is its
/// `at` (moved only
/// when a response the API actually served completes), `noticed_at` is
/// `noticedAt` (moved only when the cold notice fires — the decision
/// unit keeps the two questions apart),
/// `forced_from`/`forced_to` are `forced:
/// {from, to}` (the sticky-upgrade record, consulted while the lane's cache
/// may still be warm).
#[derive(Debug, Clone, PartialEq)]
pub struct Lane {
    pub key: String,
    pub session_id: Option<String>,
    pub tools_hash: Option<String>,
    pub updated_ms: i64,
    pub prompt_tokens: Option<i64>,
    pub ttl: Option<i64>,
    pub ping: Option<bool>,
    /// When the cold notice last fired for this lane's idle spell; never
    /// moves with `updated_ms` (a notice resets nothing it measures).
    pub noticed_at: Option<i64>,
    /// The sticky upgrade: the model this lane's conversation was moved off.
    pub forced_from: Option<String>,
    /// The model its cache now lives on.
    pub forced_to: Option<String>,
    /// Generation for compare-and-swap pin replacement.
    pub pin_generation: i64,
    /// Complete policy pin, or `None` when every stored pin field is NULL.
    pub pin: Option<LanePin>,
}

/// The complete route identity pinned to one lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanePin {
    pub policy_revision_id: i64,
    pub policy_route_id: RouteId,
    pub route_fingerprint: RouteFingerprint,
    pub candidate_id: CandidateId,
    pub binding_id: BindingId,
    /// The account that served. Part of the cache identity: a prompt cache
    /// belongs to the account that wrote it, so a lane that moved account
    /// reads nothing of the cache under its new one. `None` where the
    /// provider has no accounts Toker can tell apart, and on a pin written
    /// before accounts were recorded; such a pin claims no cache identity
    /// beyond its binding and model.
    pub account_number: Option<i64>,
    pub baseline_model: String,
    pub effective_model: String,
}

/// Result of a generation-and-prior-identity compare-and-swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanePinCasResult {
    Updated { generation: i64 },
    Conflict,
}

/// Immutable policy revision source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyRevisionSource {
    Legacy,
    Draft,
    Reactivation,
    /// An edit applied to the active revision's document and activated.
    Save,
}

impl PolicyRevisionSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Draft => "draft",
            Self::Reactivation => "reactivation",
            Self::Save => "save",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "legacy" => Some(Self::Legacy),
            "draft" => Some(Self::Draft),
            "reactivation" => Some(Self::Reactivation),
            "save" => Some(Self::Save),
            _ => None,
        }
    }
}

/// One immutable stored policy revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyRevision {
    pub revision_id: i64,
    pub created_ms: i64,
    pub activated_ms: i64,
    pub description: Option<String>,
    pub activation_scope: ActivationScope,
    pub forced_cache_transition: bool,
    pub source: PolicyRevisionSource,
    pub parent_revision_id: Option<i64>,
    pub document: ValidatedPolicyDocument,
    pub fingerprint: PolicyFingerprint,
}

/// Input for inserting a revision without changing routing authority.
#[derive(Debug, Clone)]
pub struct NewPolicyRevision<'a> {
    pub created_ms: i64,
    pub activated_ms: i64,
    pub description: Option<&'a str>,
    pub activation_scope: ActivationScope,
    pub forced_cache_transition: bool,
    pub source: PolicyRevisionSource,
    pub parent_revision_id: Option<i64>,
    pub document: &'a ValidatedPolicyDocument,
}

/// An activation scope widened after the revision was activated, with the
/// consent given for it (spec: RREV#widening-an-activation). The revision
/// itself is never edited, so this lies beside it and the next activation
/// clears it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivationWidening {
    pub scope: ActivationScope,
    pub forced_cache_transition: bool,
    pub widened_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingPolicyState {
    /// NULL until the server first establishes revision one.
    pub active_revision_id: Option<i64>,
    pub legacy_imported_ms: Option<i64>,
    /// The active revision's scope as last widened, where it was.
    pub widened: Option<ActivationWidening>,
}

impl RoutingPolicyState {
    /// The scope and force consent the active revision's lanes are governed
    /// by: its own, as last widened where it was
    /// (spec: RPIN#revision-selection).
    pub fn effective_activation(&self, revision: &PolicyRevision) -> (ActivationScope, bool) {
        match self.widened {
            Some(widening) => (widening.scope, widening.forced_cache_transition),
            None => (revision.activation_scope, revision.forced_cache_transition),
        }
    }
}

/// How an attempt to widen the active revision's scope came out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WidenResult {
    Widened {
        state: RoutingPolicyState,
    },
    /// The active revision is not the one the widening was made against.
    Conflict {
        current: RoutingPolicyState,
    },
    /// The requested scope is narrower than, or no wider than, the one
    /// standing, and carries no consent it lacked.
    NotAWidening {
        current_scope: ActivationScope,
        current_forced_cache_transition: bool,
    },
}

/// Result of the one-time legacy-policy ownership transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevisionOneActivation {
    Activated {
        revision: PolicyRevision,
        state: RoutingPolicyState,
        publications: Vec<RoutingPublication>,
    },
    AlreadyActive {
        revision: PolicyRevision,
        state: RoutingPolicyState,
        publications: Vec<RoutingPublication>,
    },
    /// No revision is active and the caller supplied no first revision, so
    /// nothing was changed.
    NeedsDocument,
}

/// Input to atomically install a new live policy revision and publication
/// desires. The Store deliberately does not compile the document: the caller
/// must compile it against the same immutable inventory it will install.
#[derive(Debug, Clone)]
pub struct ActivatePolicyRevision<'a> {
    pub created_ms: i64,
    pub description: Option<&'a str>,
    pub activation_scope: ActivationScope,
    pub forced_cache_transition: bool,
    pub source: PolicyRevisionSource,
    /// The selected history revision for reactivation, or no explicit parent
    /// for a direct apply.
    pub parent_revision_id: Option<i64>,
    /// The active revision observed while compilation ran. The Store compares
    /// it in the activation transaction so a control operation cannot install
    /// against a stale live base.
    pub expected_active_revision_id: i64,
    pub document: &'a ValidatedPolicyDocument,
    pub desired_inventory_generation: i64,
    pub publications: &'a [InitialPublication],
}

/// The committed policy revision and the state projected from the same
/// transaction. Callers may swap their runtime only after receiving this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivatedPolicyRevision {
    pub revision: PolicyRevision,
    pub state: RoutingPolicyState,
    pub publications: Vec<RoutingPublication>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationState {
    Pending,
    Applied,
    Failed,
    Unsupported,
}

impl PublicationState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applied => "applied",
            Self::Failed => "failed",
            Self::Unsupported => "unsupported",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "applied" => Some(Self::Applied),
            "failed" => Some(Self::Failed),
            "unsupported" => Some(Self::Unsupported),
            _ => None,
        }
    }
}

/// Publication adapter identity, validated with the stable policy-id grammar.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PublicationAdapter(crate::policy::StableId);

impl PublicationAdapter {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        crate::policy::StableId::new(value.into())
            .map(Self)
            .map_err(|error| {
                Error::InvalidRoutingState(format!("invalid publication adapter: {error}"))
            })
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// Typed frontend scope plus publication adapter identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationKey {
    scope: crate::policy::FrontendScope,
    adapter: PublicationAdapter,
}

impl PublicationKey {
    pub fn new(scope: crate::policy::FrontendScope, adapter: PublicationAdapter) -> Self {
        Self { scope, adapter }
    }

    pub fn scope(&self) -> &crate::policy::FrontendScope {
        &self.scope
    }

    pub fn adapter(&self) -> &PublicationAdapter {
        &self.adapter
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitialPublication {
    pub key: PublicationKey,
    pub state: PublicationState,
}

/// The publication rows a freshly activated revision starts with. Startup and
/// setup both establish revision one, so both take the rows from here.
pub fn publication_targets() -> Result<Vec<InitialPublication>> {
    use crate::policy::{FrontendProtocol, FrontendScope};

    let protocol = |protocol| FrontendScope::Protocol { protocol };
    Ok(vec![
        InitialPublication {
            key: PublicationKey::new(
                protocol(FrontendProtocol::OpenaiChat),
                PublicationAdapter::new("dynamic_chat_models")?,
            ),
            state: PublicationState::Applied,
        },
        InitialPublication {
            key: PublicationKey::new(
                protocol(FrontendProtocol::OpenaiResponses),
                PublicationAdapter::new("dynamic_responses_models")?,
            ),
            state: PublicationState::Applied,
        },
        InitialPublication {
            key: PublicationKey::new(
                protocol(FrontendProtocol::AnthropicMessages),
                PublicationAdapter::new("claude_settings")?,
            ),
            state: PublicationState::Pending,
        },
    ])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingPublication {
    pub key: PublicationKey,
    pub desired_revision_id: i64,
    pub applied_revision_id: Option<i64>,
    pub desired_inventory_generation: i64,
    pub applied_inventory_generation: Option<i64>,
    pub applied_identities: Option<BTreeSet<String>>,
    pub state: PublicationState,
    pub detail_code: Option<StableId>,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationResultUpdate {
    pub state: PublicationState,
    pub applied_identities: Option<BTreeSet<String>>,
    pub detail_code: Option<StableId>,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicationCasResult {
    Updated,
    Conflict { current: Option<RoutingPublication> },
}

/// Maximum encoded size of the exact external row identities a publication
/// adapter owns. This is deliberately much smaller than a policy document: it
/// is only a bounded set of setting keys, not arbitrary adapter data.
const MAX_PUBLICATION_IDENTITIES_JSON_BYTES: usize = 64 * 1024;

/// Binding-scoped model evidence. Days are a sorted canonical local-day set.
#[derive(Debug, Clone, PartialEq)]
pub struct BindingLearning {
    pub binding_id: BindingId,
    pub model_id: String,
    pub days: BTreeSet<String>,
    pub max_prompt: Option<i64>,
    pub explicitly_promoted: bool,
    pub updated_ms: i64,
}

/// Merge input.
#[derive(Debug, Clone, PartialEq)]
pub struct BindingLearningUpdate {
    pub binding_id: BindingId,
    pub model_id: String,
    pub days: BTreeSet<String>,
    pub max_prompt: Option<i64>,
    pub explicitly_promoted: bool,
    pub updated_ms: i64,
}

/// One learned-model row: days served and max prompt observed for an exact
/// model identity, plus its hand-verified context-window catalogue entry
/// .
#[derive(Debug, Clone, PartialEq)]
pub struct ModelEntry {
    pub model_id: String,
    /// Days-served history as JSON (list of day markers).
    pub days_json: Option<Value>,
    /// Max prompt observed for this identity; rewrites never exceed it.
    pub max_prompt: Option<i64>,
    /// Context-window catalogue entry as JSON.
    pub context_window_json: Option<Value>,
}

/// One allowance row: a quota window opened against a meter, keyed by the
/// reset value that ends it. A row
/// whose reset has passed is inert, since no live meter reports it, and
/// [`prune_allowances`] deletes it on the server's state-prune tick.
#[derive(Debug, Clone, PartialEq)]
pub struct Allowance {
    pub session_id: String,
    /// The account whose window this releases. A release spends one
    /// account's quota and never another's (spec: ACCT#what-is-attributed-to-an-account).
    pub account_number: i64,
    pub meter: String,
    pub reset_value: i64,
    /// What the release covers: overage, or only the plan's own room.
    pub release: Release,
}

/// The `release` column's spelling.
fn release_name(release: Release) -> &'static str {
    match release {
        Release::Overage => "overage",
        Release::Plan => "plan",
    }
}

/// One ping run. Every field but `ts_ms` is
/// `None` on a row written before migration v4 added it.
#[derive(Debug, Clone, PartialEq)]
pub struct PingRecord {
    /// Row id, assigned on insert; `None` while inserting.
    pub id: Option<i64>,
    pub ts_ms: i64,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<i64>,
    /// The predicted boundary: `floor(fire time, 10 min) + 5 h`.
    pub boundary_ms: Option<i64>,
    /// The slot this run served, `hh:mm`.
    pub slot: Option<String>,
    /// What the run did.
    pub action: Option<PingAction>,
    /// The boundary the ledger reported back: for a ping, the 5-hour
    /// reset on the rows it produced; for a skip, the reset of the window
    /// already open. `None` when the ledger could not say.
    pub observed_ms: Option<i64>,
    /// Whether the observed boundary matched the prediction; `None` when
    /// there was nothing to compare.
    pub verified: Option<bool>,
    /// The decision to ping was taken for want of any meter reading,
    /// not because one showed no window open.
    pub assumed: Option<bool>,
    /// The account whose window this run opened, or found already open.
    /// Read off the rows the ping produced rather than chosen in advance,
    /// because the account a ping lands on is the one its credential
    /// belongs to. `None` when nothing landed, when the row named no
    /// account, or on a run recorded before accounts were.
    pub account_number: Option<i64>,
}

/// What one ping run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PingAction {
    /// Sent the client request.
    Ping,
    /// A window was already open, so nothing was sent.
    Skip,
    /// The client did not run or did not exit cleanly.
    Failed,
}

impl PingAction {
    /// The stored string form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Skip => "skip",
            Self::Failed => "failed",
        }
    }

    /// Parse the stored string form; `None` for unknown values.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ping" => Some(Self::Ping),
            "skip" => Some(Self::Skip),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// The last meter snapshot for one meter-source backend. Every response updates
/// its own provider's slot when the
/// backend has meters, so a spent reading stops counting once its
/// window's reset passes. Keyed by provider id (migration v3): the
/// anthropic sub's quota shape and the codex sub's usage-limit shape
/// never overwrite each other, and the quota gate reads only its own.
#[derive(Debug, Clone, PartialEq)]
pub struct MetersSnapshot {
    pub updated_ms: i64,
    /// Meter-source-dependent snapshot, stored whole as JSON.
    pub snapshot: Value,
}

pub(super) fn insert_lane_if_absent(conn: &Connection, lane: &Lane) -> Result<()> {
    conn.execute(
        "INSERT INTO lanes (key, session_id, tools_hash, updated_ms, prompt_tokens, ttl, ping,
                            noticed_at, forced_from, forced_to)
         VALUES (:key, :session_id, :tools_hash, :updated_ms, :prompt_tokens, :ttl, :ping,
                 :noticed_at, :forced_from, :forced_to)
         ON CONFLICT (key) DO NOTHING",
        rusqlite::named_params! {
            ":key": lane.key,
            ":session_id": lane.session_id,
            ":tools_hash": lane.tools_hash,
            ":updated_ms": lane.updated_ms,
            ":prompt_tokens": lane.prompt_tokens,
            ":ttl": lane.ttl,
            ":ping": lane.ping,
            ":noticed_at": lane.noticed_at,
            ":forced_from": lane.forced_from,
            ":forced_to": lane.forced_to,
        },
    )?;
    Ok(())
}

pub(super) fn upsert_lane(conn: &Connection, lane: &Lane) -> Result<()> {
    // Ordinary lane observations intentionally omit every pin column. On an
    // existing row this preserves a concurrent routing decision and its CAS
    // generation; on a new row the schema supplies generation zero and NULLs.
    conn.execute(
        "INSERT INTO lanes (key, session_id, tools_hash, updated_ms, prompt_tokens, ttl, ping,
                            noticed_at, forced_from, forced_to)
         VALUES (:key, :session_id, :tools_hash, :updated_ms, :prompt_tokens, :ttl, :ping,
                 :noticed_at, :forced_from, :forced_to)
         ON CONFLICT (key) DO UPDATE SET
             session_id = excluded.session_id,
             tools_hash = excluded.tools_hash,
             updated_ms = excluded.updated_ms,
             prompt_tokens = excluded.prompt_tokens,
             ttl = excluded.ttl,
             ping = excluded.ping,
             noticed_at = excluded.noticed_at,
             forced_from = excluded.forced_from,
             forced_to = excluded.forced_to",
        rusqlite::named_params! {
            ":key": lane.key,
            ":session_id": lane.session_id,
            ":tools_hash": lane.tools_hash,
            ":updated_ms": lane.updated_ms,
            ":prompt_tokens": lane.prompt_tokens,
            ":ttl": lane.ttl,
            ":ping": lane.ping,
            ":noticed_at": lane.noticed_at,
            ":forced_from": lane.forced_from,
            ":forced_to": lane.forced_to,
        },
    )?;
    Ok(())
}

const LANE_COLUMNS: &str = "key, session_id, tools_hash, updated_ms, prompt_tokens, ttl, ping,
    noticed_at, forced_from, forced_to, pin_generation, policy_revision_id,
    policy_route_id, route_fingerprint, candidate_id, binding_id, account_number,
    baseline_model, effective_model";

pub(super) fn load_lane(conn: &Connection, key: &str) -> Result<Option<Lane>> {
    row_of(
        conn,
        &format!("SELECT {LANE_COLUMNS} FROM lanes WHERE key = ?1"),
        [key],
        read_lane,
    )
}

pub(super) fn load_lanes(conn: &Connection) -> Result<Vec<Lane>> {
    rows_of(
        conn,
        &format!("SELECT {LANE_COLUMNS} FROM lanes ORDER BY key"),
        [],
        read_lane,
    )
}

/// One session's lanes, newest first (`updated_ms` descending, key as the
/// tie-break so the order is stable). Exact on the `session_id` column,
/// never a prefix of the key: "s1" must not collect "s10"'s lanes.
pub(super) fn load_session_lanes(conn: &Connection, session_id: &str) -> Result<Vec<Lane>> {
    rows_of(
        conn,
        &format!(
            "SELECT {LANE_COLUMNS} FROM lanes
             WHERE session_id = ?1 ORDER BY updated_ms DESC, key"
        ),
        [session_id],
        read_lane,
    )
}

fn read_lane(row: &rusqlite::Row<'_>) -> Result<Lane> {
    let revision: Option<i64> = row.get("policy_revision_id")?;
    let route: Option<String> = row.get("policy_route_id")?;
    let fingerprint: Option<String> = row.get("route_fingerprint")?;
    let candidate: Option<String> = row.get("candidate_id")?;
    let binding: Option<String> = row.get("binding_id")?;
    let baseline: Option<String> = row.get("baseline_model")?;
    let effective: Option<String> = row.get("effective_model")?;
    let pin = match (
        revision,
        route,
        fingerprint,
        candidate,
        binding,
        baseline,
        effective,
    ) {
        (None, None, None, None, None, None, None) => None,
        (
            Some(policy_revision_id),
            Some(route),
            Some(fingerprint),
            Some(candidate),
            Some(binding),
            Some(baseline_model),
            Some(effective_model),
        ) => Some(LanePin {
            policy_revision_id,
            policy_route_id: parse_id("lanes.policy_route_id", route)?,
            route_fingerprint: parse_id("lanes.route_fingerprint", fingerprint)?,
            candidate_id: parse_id("lanes.candidate_id", candidate)?,
            binding_id: parse_id("lanes.binding_id", binding)?,
            account_number: row.get("account_number")?,
            baseline_model,
            effective_model,
        }),
        _ => {
            return Err(Error::InvalidRoutingState(
                "lane pin fields must be all NULL or all present".into(),
            ));
        }
    };
    Ok(Lane {
        key: row.get("key")?,
        session_id: row.get("session_id")?,
        tools_hash: row.get("tools_hash")?,
        updated_ms: row.get("updated_ms")?,
        prompt_tokens: row.get("prompt_tokens")?,
        ttl: row.get("ttl")?,
        ping: row.get("ping")?,
        noticed_at: row.get("noticed_at")?,
        forced_from: row.get("forced_from")?,
        forced_to: row.get("forced_to")?,
        pin_generation: row.get("pin_generation")?,
        pin,
    })
}

pub(super) fn compare_and_swap_lane_pin(
    conn: &Connection,
    key: &str,
    expected_generation: i64,
    expected_pin: Option<&LanePin>,
    new_pin: Option<&LanePin>,
) -> Result<LanePinCasResult> {
    if expected_generation < 0 {
        return Err(Error::InvalidRoutingState(
            "negative lane pin generation".into(),
        ));
    }
    let current = load_lane(conn, key)?;
    let Some(current) = current else {
        return Ok(LanePinCasResult::Conflict);
    };
    if current.pin_generation != expected_generation || current.pin.as_ref() != expected_pin {
        return Ok(LanePinCasResult::Conflict);
    }
    let new = StoredPinParts::from(new_pin);
    let changed = conn.execute(
        "UPDATE lanes SET pin_generation = pin_generation + 1,
             policy_revision_id = ?1, policy_route_id = ?2, route_fingerprint = ?3,
             candidate_id = ?4, binding_id = ?5, account_number = ?6,
             baseline_model = ?7, effective_model = ?8
         WHERE key = ?9 AND pin_generation = ?10
           AND policy_revision_id IS ?11 AND policy_route_id IS ?12
           AND route_fingerprint IS ?13 AND candidate_id IS ?14
           AND binding_id IS ?15 AND account_number IS ?16
           AND baseline_model IS ?17 AND effective_model IS ?18",
        rusqlite::params![
            new.revision,
            new.route,
            new.fingerprint,
            new.candidate,
            new.binding,
            new.account,
            new.baseline,
            new.effective,
            key,
            expected_generation,
            expected_pin.map(|p| p.policy_revision_id),
            expected_pin.map(|p| p.policy_route_id.as_str()),
            expected_pin.map(|p| p.route_fingerprint.to_string()),
            expected_pin.map(|p| p.candidate_id.as_str()),
            expected_pin.map(|p| p.binding_id.as_str()),
            expected_pin.and_then(|p| p.account_number),
            expected_pin.map(|p| p.baseline_model.as_str()),
            expected_pin.map(|p| p.effective_model.as_str()),
        ],
    )?;
    if changed == 1 {
        Ok(LanePinCasResult::Updated {
            generation: expected_generation + 1,
        })
    } else {
        Ok(LanePinCasResult::Conflict)
    }
}

struct StoredPinParts<'a> {
    revision: Option<i64>,
    route: Option<&'a str>,
    fingerprint: Option<String>,
    candidate: Option<&'a str>,
    binding: Option<&'a str>,
    /// Already flat: a pin that names no account and no pin at all both
    /// store NULL here.
    account: Option<i64>,
    baseline: Option<&'a str>,
    effective: Option<&'a str>,
}

impl<'a> From<Option<&'a LanePin>> for StoredPinParts<'a> {
    fn from(pin: Option<&'a LanePin>) -> Self {
        match pin {
            Some(pin) => Self {
                revision: Some(pin.policy_revision_id),
                route: Some(pin.policy_route_id.as_str()),
                fingerprint: Some(pin.route_fingerprint.to_string()),
                candidate: Some(pin.candidate_id.as_str()),
                binding: Some(pin.binding_id.as_str()),
                account: pin.account_number,
                baseline: Some(&pin.baseline_model),
                effective: Some(&pin.effective_model),
            },
            None => Self {
                revision: None,
                route: None,
                fingerprint: None,
                candidate: None,
                binding: None,
                account: None,
                baseline: None,
                effective: None,
            },
        }
    }
}

fn parse_id<T>(column: &'static str, value: String) -> Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|error| Error::InvalidRoutingState(format!("invalid {column} {value:?}: {error}")))
}

fn activation_scope_name(scope: ActivationScope) -> &'static str {
    match scope {
        ActivationScope::NewLanes => "new_lanes",
        ActivationScope::NewAndColdLanes => "new_and_cold_lanes",
        ActivationScope::Immediate => "immediate",
    }
}

fn parse_activation_scope(value: &str) -> Option<ActivationScope> {
    match value {
        "new_lanes" => Some(ActivationScope::NewLanes),
        "new_and_cold_lanes" => Some(ActivationScope::NewAndColdLanes),
        "immediate" => Some(ActivationScope::Immediate),
        _ => None,
    }
}

fn parse_required<T>(
    column: &'static str,
    value: String,
    parse: fn(&str) -> Option<T>,
) -> Result<T> {
    parse(&value).ok_or(Error::UnknownDbValue { column, value })
}

fn validated_stored_document(
    json: String,
    fingerprint: String,
) -> Result<(ValidatedPolicyDocument, PolicyFingerprint)> {
    let document = decode_policy(json.as_bytes(), CanonicalLimits::default())
        .map_err(|error| Error::InvalidPolicyDocument(error.to_string()))?;
    let canonical =
        policy_bytes(&document).map_err(|error| Error::InvalidPolicyDocument(error.to_string()))?;
    if canonical != json.as_bytes() {
        return Err(Error::InvalidPolicyDocument(
            "stored policy JSON is not canonical".into(),
        ));
    }
    let stored: PolicyFingerprint = parse_id("policy fingerprint", fingerprint)?;
    let calculated = policy_fingerprint(&document)
        .map_err(|error| Error::InvalidPolicyDocument(error.to_string()))?;
    if stored != calculated {
        return Err(Error::InvalidPolicyDocument(
            "stored policy fingerprint does not match document".into(),
        ));
    }
    Ok((document, stored))
}

pub(super) fn insert_policy_revision(
    conn: &Connection,
    revision: &NewPolicyRevision<'_>,
) -> Result<PolicyRevision> {
    let document = policy_bytes(revision.document)
        .map_err(|error| Error::InvalidPolicyDocument(error.to_string()))?;
    let document_json = String::from_utf8(document)
        .map_err(|error| Error::InvalidPolicyDocument(error.to_string()))?;
    let fingerprint = policy_fingerprint(revision.document)
        .map_err(|error| Error::InvalidPolicyDocument(error.to_string()))?;
    // `compiler_origin` is left to its `authored` default: every revision
    // compiles the same way, and the column only marks the generated
    // rows an earlier version wrote, for retirement to find.
    conn.execute(
        "INSERT INTO routing_policy_revisions
         (created_ms, activated_ms, description, activation_scope,
          forced_cache_transition, source, parent_revision_id, document_json,
          fingerprint)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            revision.created_ms,
            revision.activated_ms,
            revision.description,
            activation_scope_name(revision.activation_scope),
            revision.forced_cache_transition,
            revision.source.as_str(),
            revision.parent_revision_id,
            document_json,
            fingerprint.to_string(),
        ],
    )?;
    load_policy_revision(conn, conn.last_insert_rowid())?
        .ok_or_else(|| Error::InvalidRoutingState("inserted policy revision disappeared".into()))
}

/// Activate revision one, optionally importing a configuration alongside it.
///
/// `configuration` is the file's settings as rows. It is written inside this
/// transaction so a machine moving off its file does both or neither
/// (spec: RMIG#migrating-the-file-and-the-store-together), and it is written
/// on the already-active path too: a machine whose routing moved earlier
/// still has a file full of providers to import.
pub(super) fn activate_revision_one(
    conn: &mut Connection,
    document: Option<&ValidatedPolicyDocument>,
    activated_ms: i64,
    desired_inventory_generation: i64,
    publications: &[InitialPublication],
    configuration: Option<&super::Configuration>,
) -> Result<RevisionOneActivation> {
    if desired_inventory_generation < 0 {
        return Err(Error::InvalidRoutingState(
            "negative inventory generation".into(),
        ));
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let retired = retire_generated_revisions(&tx)?;
    let current = load_routing_policy_state(&tx)?;
    if let Some(revision_id) = current.active_revision_id {
        let revision = load_policy_revision(&tx, revision_id)?.ok_or_else(|| {
            Error::InvalidRoutingState(format!(
                "active policy revision {revision_id} does not exist"
            ))
        })?;
        repoint_retired_publications(&tx, &retired, revision_id)?;
        let publications = load_publications(&tx)?;
        if let Some(configuration) = configuration {
            super::config::write_configuration(&tx, configuration)?;
        }
        tx.commit()?;
        return Ok(RevisionOneActivation::AlreadyActive {
            revision,
            state: current,
            publications,
        });
    }
    if current.legacy_imported_ms.is_some() {
        return Err(Error::InvalidRoutingState(
            "legacy import marker exists without an active revision".into(),
        ));
    }
    // Dropping the transaction rolls back any retirement with it, so a
    // machine that cannot build its first revision keeps what it had.
    let Some(document) = document else {
        return Ok(RevisionOneActivation::NeedsDocument);
    };

    if let Some(configuration) = configuration {
        super::config::write_configuration(&tx, configuration)?;
    }

    let revision = insert_policy_revision(
        &tx,
        &NewPolicyRevision {
            created_ms: activated_ms,
            activated_ms,
            description: Some("Generated first revision"),
            activation_scope: ActivationScope::NewLanes,
            forced_cache_transition: false,
            source: PolicyRevisionSource::Legacy,
            parent_revision_id: None,
            document,
        },
    )?;
    let changed = tx.execute(
        "UPDATE routing_policy_state SET active_revision_id = ?1,
             legacy_imported_ms = ?2,
             widened_scope = NULL, widened_forced_cache_transition = NULL,
             widened_ms = NULL
         WHERE id = 1 AND active_revision_id IS NULL AND legacy_imported_ms IS NULL",
        rusqlite::params![revision.revision_id, activated_ms],
    )?;
    if changed != 1 {
        return Err(Error::InvalidRoutingState(
            "revision-one ownership state changed during activation".into(),
        ));
    }
    for publication in publications {
        set_publication_desired(
            &tx,
            &publication.key,
            revision.revision_id,
            desired_inventory_generation,
            activated_ms,
        )?;
        if publication.state == PublicationState::Applied {
            let result = compare_and_swap_publication_result(
                &tx,
                &publication.key,
                revision.revision_id,
                desired_inventory_generation,
                &PublicationResultUpdate {
                    state: PublicationState::Applied,
                    applied_identities: Some(BTreeSet::new()),
                    detail_code: None,
                    updated_ms: activated_ms,
                },
            )?;
            if result != PublicationCasResult::Updated {
                return Err(Error::InvalidRoutingState(
                    "initial publication state changed during activation".into(),
                ));
            }
        } else if publication.state != PublicationState::Pending {
            return Err(Error::InvalidRoutingState(
                "initial publication must be pending or applied".into(),
            ));
        }
    }
    repoint_retired_publications(&tx, &retired, revision.revision_id)?;
    let state = load_routing_policy_state(&tx)?;
    let publications = load_publications(&tx)?;
    tx.commit()?;
    Ok(RevisionOneActivation::Activated {
        revision,
        state,
        publications,
    })
}

/// Delete every revision an earlier version stored with generated-only
/// semantics, which no longer decode, and return their ids.
///
/// Runs inside the first-revision transaction, before anything reads the
/// pointer. Lanes pinned to one lose their pin. Where the pointer named one,
/// the pointer, the widening and the import marker are cleared so the caller
/// establishes the first revision afresh.
// spec: RMIG#retiring-generated-revisions
fn retire_generated_revisions(conn: &Connection) -> Result<Vec<i64>> {
    let retired: Vec<i64> = conn
        .prepare(
            "SELECT revision_id FROM routing_policy_revisions
             WHERE compiler_origin = 'generated_revision_one' ORDER BY revision_id",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    if retired.is_empty() {
        return Ok(retired);
    }
    // A lane pinned to a retired revision is unpinned whatever its
    // temperature: one cache rebuild on a lane that happened to be warm
    // costs less than holding the service down until the prune drops it.
    // The generation moves so a compare-and-swap built on the old pin fails.
    conn.execute(
        "UPDATE lanes SET pin_generation = pin_generation + 1,
             policy_revision_id = NULL, policy_route_id = NULL,
             route_fingerprint = NULL, candidate_id = NULL, binding_id = NULL,
             baseline_model = NULL, effective_model = NULL
         WHERE policy_revision_id IN (SELECT value FROM json_each(?1))",
        [id_list(&retired)],
    )?;

    // The rows still referring to the retired revisions are repointed or
    // cleared below and by the caller, all before this transaction commits.
    conn.pragma_update(None, "defer_foreign_keys", true)?;
    // The draft columns are no longer read or written, but a draft an earlier
    // version left on a retired revision still names it by foreign key, so
    // it goes with the revision.
    conn.execute(
        "UPDATE routing_policy_state SET
             draft_base_revision_id = NULL, draft_document_json = NULL,
             draft_fingerprint = NULL, draft_updated_ms = NULL
         WHERE id = 1 AND draft_base_revision_id IN (SELECT value FROM json_each(?1))",
        [id_list(&retired)],
    )?;
    conn.execute(
        "UPDATE routing_policy_state SET active_revision_id = NULL,
             legacy_imported_ms = NULL,
             widened_scope = NULL, widened_forced_cache_transition = NULL,
             widened_ms = NULL
         WHERE id = 1 AND active_revision_id IN (SELECT value FROM json_each(?1))",
        [id_list(&retired)],
    )?;
    // Only a generated revision derives from a generated one, but a lineage
    // pointer to a deleted row cannot be kept whatever wrote it.
    conn.execute(
        "UPDATE routing_policy_revisions SET parent_revision_id = NULL
         WHERE parent_revision_id IN (SELECT value FROM json_each(?1))",
        [id_list(&retired)],
    )?;
    conn.execute(
        "DELETE FROM routing_policy_revisions
         WHERE revision_id IN (SELECT value FROM json_each(?1))",
        [id_list(&retired)],
    )?;
    Ok(retired)
}

/// Point every publication that named a retired revision at `active`, to
/// be applied afresh. An applied publication of a retired revision has
/// nothing left to stand for, so it is cleared as well.
fn repoint_retired_publications(conn: &Connection, retired: &[i64], active: i64) -> Result<()> {
    if retired.is_empty() {
        return Ok(());
    }
    conn.execute(
        "UPDATE routing_publications SET applied_revision_id = NULL,
             applied_inventory_generation = NULL,
             state = CASE WHEN state = 'applied' THEN 'pending' ELSE state END
         WHERE applied_revision_id IN (SELECT value FROM json_each(?1))",
        [id_list(retired)],
    )?;
    conn.execute(
        "UPDATE routing_publications SET desired_revision_id = ?2,
             state = CASE WHEN state = 'applied' THEN 'pending' ELSE state END
         WHERE desired_revision_id IN (SELECT value FROM json_each(?1))",
        rusqlite::params![id_list(retired), active],
    )?;
    Ok(())
}

/// A list of revision ids as the JSON array `json_each` reads.
fn id_list(ids: &[i64]) -> String {
    serde_json::to_string(ids).expect("integers serialise")
}

/// Insert, activate, and hand publication desire to every adapter in one
/// immediate transaction. The active revision, its publications, and the
/// response projection therefore cannot disagree after the transaction commits.
pub(super) fn activate_policy_revision(
    conn: &mut Connection,
    activation: &ActivatePolicyRevision<'_>,
) -> Result<ActivatedPolicyRevision> {
    if activation.desired_inventory_generation < 0 {
        return Err(Error::InvalidRoutingState(
            "negative inventory generation".into(),
        ));
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = load_routing_policy_state(&tx)?;
    let active_revision_id = current.active_revision_id.ok_or_else(|| {
        Error::InvalidRoutingState(
            "cannot activate a policy without an active base revision".into(),
        )
    })?;
    if active_revision_id != activation.expected_active_revision_id {
        return Err(Error::InvalidRoutingState(
            "active policy revision changed before activation".into(),
        ));
    }
    if let Some(parent_revision_id) = activation.parent_revision_id
        && load_policy_revision(&tx, parent_revision_id)?.is_none()
    {
        return Err(Error::InvalidRoutingState(format!(
            "policy revision parent {parent_revision_id} does not exist"
        )));
    }
    let revision = insert_policy_revision(
        &tx,
        &NewPolicyRevision {
            created_ms: activation.created_ms,
            activated_ms: activation.created_ms,
            description: activation.description,
            activation_scope: activation.activation_scope,
            forced_cache_transition: activation.forced_cache_transition,
            source: activation.source,
            parent_revision_id: activation.parent_revision_id.or(Some(active_revision_id)),
            document: activation.document,
        },
    )?;
    // A widening belonged to the revision it widened, so the new one starts
    // from its own scope.
    let changed = tx.execute(
        "UPDATE routing_policy_state SET active_revision_id = ?1,
             widened_scope = NULL, widened_forced_cache_transition = NULL,
             widened_ms = NULL
         WHERE id = 1 AND active_revision_id = ?2",
        rusqlite::params![revision.revision_id, active_revision_id],
    )?;
    if changed != 1 {
        return Err(Error::InvalidRoutingState(
            "active policy revision changed during activation".into(),
        ));
    }
    for publication in activation.publications {
        set_publication_desired(
            &tx,
            &publication.key,
            revision.revision_id,
            activation.desired_inventory_generation,
            activation.created_ms,
        )?;
        if publication.state == PublicationState::Applied {
            let result = compare_and_swap_publication_result(
                &tx,
                &publication.key,
                revision.revision_id,
                activation.desired_inventory_generation,
                &PublicationResultUpdate {
                    state: PublicationState::Applied,
                    applied_identities: Some(BTreeSet::new()),
                    detail_code: None,
                    updated_ms: activation.created_ms,
                },
            )?;
            if result != PublicationCasResult::Updated {
                return Err(Error::InvalidRoutingState(
                    "initial publication state changed during activation".into(),
                ));
            }
        } else if publication.state != PublicationState::Pending {
            return Err(Error::InvalidRoutingState(
                "initial publication must be pending or applied".into(),
            ));
        }
    }
    let state = load_routing_policy_state(&tx)?;
    let publications = load_publications(&tx)?;
    tx.commit()?;
    Ok(ActivatedPolicyRevision {
        revision,
        state,
        publications,
    })
}

pub(super) fn load_policy_revision(
    conn: &Connection,
    revision_id: i64,
) -> Result<Option<PolicyRevision>> {
    row_of(
        conn,
        "SELECT revision_id, created_ms, activated_ms, description, activation_scope,
                forced_cache_transition, source, parent_revision_id, document_json, fingerprint
         FROM routing_policy_revisions WHERE revision_id = ?1",
        [revision_id],
        read_policy_revision,
    )
}

pub(super) fn load_policy_revisions(conn: &Connection) -> Result<Vec<PolicyRevision>> {
    rows_of(
        conn,
        "SELECT revision_id, created_ms, activated_ms, description, activation_scope,
                forced_cache_transition, source, parent_revision_id, document_json, fingerprint
         FROM routing_policy_revisions ORDER BY revision_id",
        [],
        read_policy_revision,
    )
}

fn read_policy_revision(row: &rusqlite::Row<'_>) -> Result<PolicyRevision> {
    let (document, fingerprint) =
        validated_stored_document(row.get("document_json")?, row.get("fingerprint")?)?;
    Ok(PolicyRevision {
        revision_id: row.get("revision_id")?,
        created_ms: row.get("created_ms")?,
        activated_ms: row.get("activated_ms")?,
        description: row.get("description")?,
        activation_scope: parse_required(
            "routing_policy_revisions.activation_scope",
            row.get("activation_scope")?,
            parse_activation_scope,
        )?,
        forced_cache_transition: row.get("forced_cache_transition")?,
        source: parse_required(
            "routing_policy_revisions.source",
            row.get("source")?,
            PolicyRevisionSource::parse,
        )?,
        parent_revision_id: row.get("parent_revision_id")?,
        document,
        fingerprint,
    })
}

pub(super) fn load_routing_policy_state(conn: &Connection) -> Result<RoutingPolicyState> {
    row_of(
        conn,
        "SELECT active_revision_id, legacy_imported_ms, widened_scope,
                widened_forced_cache_transition, widened_ms
         FROM routing_policy_state WHERE id = 1",
        [],
        |row| {
            let scope: Option<String> = row.get("widened_scope")?;
            let forced: Option<bool> = row.get("widened_forced_cache_transition")?;
            let widened_ms: Option<i64> = row.get("widened_ms")?;
            let widened = match (scope, forced, widened_ms) {
                (None, None, None) => None,
                (Some(scope), Some(forced_cache_transition), Some(widened_ms)) => {
                    Some(ActivationWidening {
                        scope: parse_required(
                            "routing_policy_state.widened_scope",
                            scope,
                            parse_activation_scope,
                        )?,
                        forced_cache_transition,
                        widened_ms,
                    })
                }
                _ => {
                    return Err(Error::InvalidRoutingState(
                        "widening fields must be all NULL or all present".into(),
                    ));
                }
            };
            Ok(RoutingPolicyState {
                active_revision_id: row.get("active_revision_id")?,
                legacy_imported_ms: row.get("legacy_imported_ms")?,
                widened,
            })
        },
    )?
    .ok_or_else(|| {
        Error::InvalidRoutingState("routing_policy_state singleton row is missing".into())
    })
}

/// Widen the active revision's activation scope, without creating a revision
/// (spec: RREV#widening-an-activation). One immediate transaction checks the
/// active revision, refuses a scope that is not wider and records the new
/// one, so a widening made against a revision that has since been replaced
/// changes nothing. Whether an immediate widening needs consent, and has it,
/// is the caller's to settle before this: the store holds no lane evidence.
pub(super) fn widen_activation(
    conn: &mut Connection,
    expected_active_revision_id: i64,
    scope: ActivationScope,
    forced_cache_transition: bool,
    widened_ms: i64,
) -> Result<WidenResult> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = load_routing_policy_state(&tx)?;
    let Some(active) = current.active_revision_id else {
        return Err(Error::InvalidRoutingState(
            "cannot widen without an active policy revision".into(),
        ));
    };
    if active != expected_active_revision_id {
        return Ok(WidenResult::Conflict { current });
    }
    let revision = load_policy_revision(&tx, active)?.ok_or_else(|| {
        Error::InvalidRoutingState(format!("active policy revision {active} does not exist"))
    })?;
    let (current_scope, current_forced) = current.effective_activation(&revision);
    if !is_widening(
        (current_scope, current_forced),
        (scope, forced_cache_transition),
    ) {
        return Ok(WidenResult::NotAWidening {
            current_scope,
            current_forced_cache_transition: current_forced,
        });
    }
    tx.execute(
        "UPDATE routing_policy_state SET widened_scope = ?1,
             widened_forced_cache_transition = ?2, widened_ms = ?3
         WHERE id = 1 AND active_revision_id = ?4",
        rusqlite::params![
            activation_scope_name(scope),
            forced_cache_transition,
            widened_ms,
            active
        ],
    )?;
    let state = load_routing_policy_state(&tx)?;
    tx.commit()?;
    Ok(WidenResult::Widened { state })
}

/// Whether moving from the scope and consent standing to another is a
/// widening: a wider scope, or immediate again with the consent it lacked. A
/// scope is never narrowed, and consent given is never taken back.
pub fn is_widening(
    (current, current_forced): (ActivationScope, bool),
    (scope, forced): (ActivationScope, bool),
) -> bool {
    scope_rank(scope) > scope_rank(current)
        || (scope == current && scope == ActivationScope::Immediate && forced && !current_forced)
}

/// How wide an activation scope is: the lanes it can move grow with it.
pub fn scope_rank(scope: ActivationScope) -> u8 {
    match scope {
        ActivationScope::NewLanes => 0,
        ActivationScope::NewAndColdLanes => 1,
        ActivationScope::Immediate => 2,
    }
}

fn frontend_protocol_name(protocol: crate::policy::FrontendProtocol) -> &'static str {
    match protocol {
        crate::policy::FrontendProtocol::AnthropicMessages => "anthropic_messages",
        crate::policy::FrontendProtocol::OpenaiChat => "openai_chat",
        crate::policy::FrontendProtocol::OpenaiResponses => "openai_responses",
    }
}

fn encode_publication_scope(scope: &crate::policy::FrontendScope) -> String {
    match scope {
        crate::policy::FrontendScope::Profile { profile, protocol } => {
            format!(
                "profile:{}:{}",
                profile.as_str(),
                frontend_protocol_name(*protocol)
            )
        }
        crate::policy::FrontendScope::Protocol { protocol } => {
            format!("protocol:{}", frontend_protocol_name(*protocol))
        }
    }
}

fn parse_frontend_protocol(value: &str) -> Option<crate::policy::FrontendProtocol> {
    match value {
        "anthropic_messages" => Some(crate::policy::FrontendProtocol::AnthropicMessages),
        "openai_chat" => Some(crate::policy::FrontendProtocol::OpenaiChat),
        "openai_responses" => Some(crate::policy::FrontendProtocol::OpenaiResponses),
        _ => None,
    }
}

fn decode_publication_scope(value: &str) -> Result<crate::policy::FrontendScope> {
    if let Some(protocol) = value.strip_prefix("protocol:")
        && let Some(protocol) = parse_frontend_protocol(protocol)
    {
        return Ok(crate::policy::FrontendScope::Protocol { protocol });
    }
    if let Some(rest) = value.strip_prefix("profile:") {
        let mut parts = rest.split(':');
        if let (Some(profile), Some(protocol), None) = (parts.next(), parts.next(), parts.next())
            && let Some(protocol) = parse_frontend_protocol(protocol)
            && let Ok(profile) = crate::policy::ProfileId::new(profile.to_owned())
        {
            return Ok(crate::policy::FrontendScope::Profile { profile, protocol });
        }
    }
    Err(Error::InvalidRoutingState(format!(
        "invalid publication scope key {value:?}"
    )))
}

pub(super) fn set_publication_desired(
    conn: &Connection,
    key: &PublicationKey,
    desired_revision_id: i64,
    desired_inventory_generation: i64,
    updated_ms: i64,
) -> Result<()> {
    if desired_inventory_generation < 0 {
        return Err(Error::InvalidRoutingState(
            "negative inventory generation".into(),
        ));
    }
    let scope_key = encode_publication_scope(&key.scope);
    // Restating the pair an applied publication already carries (the daily
    // catalogue refresh does) leaves it applied: nothing is left to apply.
    conn.execute(
        "INSERT INTO routing_publications
         (scope_key, adapter, desired_revision_id, desired_inventory_generation, state, updated_ms)
         VALUES (?1, ?2, ?3, ?4, 'pending', ?5)
         ON CONFLICT(scope_key, adapter) DO UPDATE SET
           desired_revision_id = excluded.desired_revision_id,
           desired_inventory_generation = excluded.desired_inventory_generation,
           state = CASE WHEN state = 'applied'
                         AND desired_revision_id = excluded.desired_revision_id
                         AND desired_inventory_generation
                             = excluded.desired_inventory_generation
                        THEN state ELSE 'pending' END,
           detail_code = CASE WHEN state = 'applied'
                               AND desired_revision_id = excluded.desired_revision_id
                               AND desired_inventory_generation
                                   = excluded.desired_inventory_generation
                              THEN detail_code ELSE NULL END,
           updated_ms = excluded.updated_ms",
        rusqlite::params![
            scope_key,
            key.adapter.as_str(),
            desired_revision_id,
            desired_inventory_generation,
            updated_ms
        ],
    )?;
    Ok(())
}

pub(super) fn set_publications_desired(
    conn: &mut Connection,
    publications: &[InitialPublication],
    desired_revision_id: i64,
    desired_inventory_generation: i64,
    updated_ms: i64,
) -> Result<Vec<RoutingPublication>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for publication in publications {
        set_publication_desired(
            &tx,
            &publication.key,
            desired_revision_id,
            desired_inventory_generation,
            updated_ms,
        )?;
        if publication.state == PublicationState::Applied {
            let result = compare_and_swap_publication_result(
                &tx,
                &publication.key,
                desired_revision_id,
                desired_inventory_generation,
                &PublicationResultUpdate {
                    state: PublicationState::Applied,
                    applied_identities: Some(BTreeSet::new()),
                    detail_code: None,
                    updated_ms,
                },
            )?;
            if result != PublicationCasResult::Updated {
                return Err(Error::InvalidRoutingState(
                    "publication state changed during desired handoff".into(),
                ));
            }
        } else if publication.state != PublicationState::Pending {
            return Err(Error::InvalidRoutingState(
                "desired publication must be pending or applied".into(),
            ));
        }
    }
    let rows = load_publications(&tx)?;
    tx.commit()?;
    Ok(rows)
}

pub(super) fn load_publication(
    conn: &Connection,
    key: &PublicationKey,
) -> Result<Option<RoutingPublication>> {
    let scope_key = encode_publication_scope(&key.scope);
    row_of(
        conn,
        "SELECT scope_key, adapter, desired_revision_id, applied_revision_id,
                desired_inventory_generation, applied_inventory_generation,
                applied_identities_json, state, detail_code, updated_ms
         FROM routing_publications WHERE scope_key = ?1 AND adapter = ?2",
        [scope_key.as_str(), key.adapter.as_str()],
        read_publication,
    )
}

pub(super) fn load_publications(conn: &Connection) -> Result<Vec<RoutingPublication>> {
    rows_of(
        conn,
        "SELECT scope_key, adapter, desired_revision_id, applied_revision_id,
                desired_inventory_generation, applied_inventory_generation,
                applied_identities_json, state, detail_code, updated_ms
         FROM routing_publications ORDER BY scope_key, adapter",
        [],
        read_publication,
    )
}

fn read_publication(row: &rusqlite::Row<'_>) -> Result<RoutingPublication> {
    let scope = decode_publication_scope(&row.get::<_, String>("scope_key")?)?;
    let adapter = PublicationAdapter::new(row.get::<_, String>("adapter")?)?;
    let key = PublicationKey::new(scope, adapter);
    let state = parse_required(
        "routing_publications.state",
        row.get("state")?,
        PublicationState::parse,
    )?;
    let desired_revision_id = row.get("desired_revision_id")?;
    let applied_revision_id = row.get("applied_revision_id")?;
    let desired_inventory_generation = row.get("desired_inventory_generation")?;
    let applied_inventory_generation = row.get("applied_inventory_generation")?;
    let applied_identities = row
        .get::<_, Option<String>>("applied_identities_json")?
        .map(|json| parse_publication_identities(&json))
        .transpose()?;
    let detail_code = row
        .get::<_, Option<String>>("detail_code")?
        .map(|value| parse_id("routing_publications.detail_code", value))
        .transpose()?;
    if state == PublicationState::Applied
        && (applied_revision_id != Some(desired_revision_id)
            || applied_inventory_generation != Some(desired_inventory_generation)
            || applied_identities.is_none())
    {
        return Err(Error::InvalidRoutingState(
            "applied publication does not carry its exact desired identity set".into(),
        ));
    }
    Ok(RoutingPublication {
        key,
        desired_revision_id,
        applied_revision_id,
        desired_inventory_generation,
        applied_inventory_generation,
        applied_identities,
        state,
        detail_code,
        updated_ms: row.get("updated_ms")?,
    })
}

pub(super) fn compare_and_swap_publication_result(
    conn: &Connection,
    key: &PublicationKey,
    expected_revision_id: i64,
    expected_inventory_generation: i64,
    update: &PublicationResultUpdate,
) -> Result<PublicationCasResult> {
    if update.state == PublicationState::Pending {
        return Err(Error::InvalidRoutingState(
            "publication result cannot be pending".into(),
        ));
    }
    if update.state == PublicationState::Applied && update.applied_identities.is_none() {
        return Err(Error::InvalidRoutingState(
            "an applied publication result needs an exact row identity set".into(),
        ));
    }
    if update.state != PublicationState::Applied && update.applied_identities.is_some() {
        return Err(Error::InvalidRoutingState(
            "only an applied result may replace publication row identities".into(),
        ));
    }
    validate_publication_identities(update.applied_identities.as_ref())?;
    let applied_identities_json = update
        .applied_identities
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let scope_key = encode_publication_scope(&key.scope);
    let changed = if update.state == PublicationState::Applied {
        conn.execute(
            "UPDATE routing_publications SET state = ?1,
                 applied_revision_id = desired_revision_id,
                 applied_inventory_generation = desired_inventory_generation,
                 applied_identities_json = ?2, detail_code = ?3, updated_ms = ?4
             WHERE scope_key = ?5 AND adapter = ?6 AND desired_revision_id = ?7
               AND desired_inventory_generation = ?8",
            rusqlite::params![
                update.state.as_str(),
                applied_identities_json,
                update.detail_code.as_ref().map(StableId::as_str),
                update.updated_ms,
                scope_key,
                key.adapter.as_str(),
                expected_revision_id,
                expected_inventory_generation
            ],
        )?
    } else {
        // A result that races a successful reconciliation of this desired
        // identity must not turn the published state back into failure.
        conn.execute(
            "UPDATE routing_publications SET state = ?1, detail_code = ?2,
                 updated_ms = ?3
             WHERE scope_key = ?4 AND adapter = ?5 AND desired_revision_id = ?6
               AND desired_inventory_generation = ?7 AND state != 'applied'",
            rusqlite::params![
                update.state.as_str(),
                update.detail_code.as_ref().map(StableId::as_str),
                update.updated_ms,
                scope_key,
                key.adapter.as_str(),
                expected_revision_id,
                expected_inventory_generation
            ],
        )?
    };
    if changed == 1 {
        Ok(PublicationCasResult::Updated)
    } else {
        Ok(PublicationCasResult::Conflict {
            current: load_publication(conn, key)?,
        })
    }
}

fn parse_publication_identities(json: &str) -> Result<BTreeSet<String>> {
    if json.len() > MAX_PUBLICATION_IDENTITIES_JSON_BYTES {
        return Err(Error::InvalidRoutingState(
            "publication row identities exceed the stored size limit".into(),
        ));
    }
    let values: Vec<String> = serde_json::from_str(json)?;
    let identities = values.iter().cloned().collect::<BTreeSet<_>>();
    if identities.len() != values.len() {
        return Err(Error::InvalidRoutingState(
            "publication row identities are not unique".into(),
        ));
    }
    if values.iter().ne(identities.iter()) {
        return Err(Error::InvalidRoutingState(
            "publication row identities are not in canonical order".into(),
        ));
    }
    validate_publication_identities(Some(&identities))?;
    Ok(identities)
}

fn validate_publication_identities(identities: Option<&BTreeSet<String>>) -> Result<()> {
    let Some(identities) = identities else {
        return Ok(());
    };
    if identities.len() > crate::policy::MAX_COLLECTION_ITEMS {
        return Err(Error::InvalidRoutingState(
            "too many publication row identities".into(),
        ));
    }
    if identities
        .iter()
        .any(|identity| identity.is_empty() || identity.len() > crate::policy::MAX_STRING_BYTES)
    {
        return Err(Error::InvalidRoutingState(
            "invalid publication row identity".into(),
        ));
    }
    let encoded = serde_json::to_vec(identities)?;
    if encoded.len() > MAX_PUBLICATION_IDENTITIES_JSON_BYTES {
        return Err(Error::InvalidRoutingState(
            "publication row identities exceed the stored size limit".into(),
        ));
    }
    Ok(())
}

fn validate_learning(update: &BindingLearningUpdate) -> Result<()> {
    if update.model_id.is_empty() {
        return Err(Error::InvalidRoutingState(
            "binding learning model id is empty".into(),
        ));
    }
    if update.max_prompt.is_some_and(|value| value < 0) {
        return Err(Error::InvalidRoutingState(
            "negative learned max prompt".into(),
        ));
    }
    if update.days.iter().any(|day| !valid_local_day(day)) {
        return Err(Error::InvalidRoutingState(
            "invalid learned local day".into(),
        ));
    }
    Ok(())
}

fn valid_local_day(day: &str) -> bool {
    let bytes = day.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
        && day.parse::<jiff::civil::Date>().is_ok()
}

pub(super) fn merge_binding_learning(
    conn: &mut Connection,
    update: &BindingLearningUpdate,
) -> Result<BindingLearning> {
    validate_learning(update)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let merged = merge_binding_learning_in_transaction(&tx, update)?;
    tx.commit()?;
    Ok(merged)
}

fn merge_binding_learning_in_transaction(
    conn: &Connection,
    update: &BindingLearningUpdate,
) -> Result<BindingLearning> {
    let existing = load_binding_learning(conn, &update.binding_id, &update.model_id)?;
    let mut days = existing
        .as_ref()
        .map_or_else(BTreeSet::new, |entry| entry.days.clone());
    days.extend(update.days.iter().cloned());
    let max_prompt = match (
        existing.as_ref().and_then(|v| v.max_prompt),
        update.max_prompt,
    ) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (some, None) | (None, some) => some,
    };
    let explicitly_promoted =
        update.explicitly_promoted || existing.as_ref().is_some_and(|v| v.explicitly_promoted);
    let updated_ms = existing
        .as_ref()
        .map_or(update.updated_ms, |v| v.updated_ms.max(update.updated_ms));
    let days_json = if days.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&days.iter().collect::<Vec<_>>())?)
    };
    conn.execute(
        "INSERT INTO learned_models_by_binding
         (binding_id, model_id, days_json, max_prompt, explicitly_promoted, updated_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(binding_id, model_id) DO UPDATE SET
           days_json = excluded.days_json, max_prompt = excluded.max_prompt,
           explicitly_promoted = excluded.explicitly_promoted, updated_ms = excluded.updated_ms",
        rusqlite::params![
            update.binding_id.as_str(),
            update.model_id,
            days_json,
            max_prompt,
            explicitly_promoted,
            updated_ms
        ],
    )?;
    Ok(BindingLearning {
        binding_id: update.binding_id.clone(),
        model_id: update.model_id.clone(),
        days,
        max_prompt,
        explicitly_promoted,
        updated_ms,
    })
}

pub(super) fn promote_binding_learning(
    conn: &mut Connection,
    binding_id: &BindingId,
    model_id: &str,
    updated_ms: i64,
) -> Result<Option<BindingLearning>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE learned_models_by_binding
         SET explicitly_promoted = 1, updated_ms = max(updated_ms, ?3)
         WHERE binding_id = ?1 AND model_id = ?2",
        rusqlite::params![binding_id.as_str(), model_id, updated_ms],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    let promoted = load_binding_learning(&tx, binding_id, model_id)?;
    tx.commit()?;
    Ok(promoted)
}

pub(super) fn load_binding_learning(
    conn: &Connection,
    binding_id: &BindingId,
    model_id: &str,
) -> Result<Option<BindingLearning>> {
    row_of(
        conn,
        "SELECT binding_id, model_id, days_json, max_prompt, explicitly_promoted,
                updated_ms
         FROM learned_models_by_binding WHERE binding_id = ?1 AND model_id = ?2",
        [binding_id.as_str(), model_id],
        read_binding_learning,
    )
}

pub(super) fn load_binding_learnings_for_model(
    conn: &Connection,
    model_id: &str,
) -> Result<Vec<BindingLearning>> {
    rows_of(
        conn,
        "SELECT binding_id, model_id, days_json, max_prompt, explicitly_promoted,
                updated_ms
         FROM learned_models_by_binding WHERE model_id = ?1 ORDER BY binding_id",
        [model_id],
        read_binding_learning,
    )
}

pub(super) fn load_all_binding_learnings(conn: &Connection) -> Result<Vec<BindingLearning>> {
    rows_of(
        conn,
        "SELECT binding_id, model_id, days_json, max_prompt, explicitly_promoted,
                updated_ms
         FROM learned_models_by_binding ORDER BY binding_id, model_id",
        [],
        read_binding_learning,
    )
}

pub(super) fn load_binding_learnings(
    conn: &Connection,
    binding_id: &BindingId,
) -> Result<Vec<BindingLearning>> {
    rows_of(
        conn,
        "SELECT binding_id, model_id, days_json, max_prompt, explicitly_promoted,
                updated_ms
         FROM learned_models_by_binding WHERE binding_id = ?1 ORDER BY model_id",
        [binding_id.as_str()],
        read_binding_learning,
    )
}

fn read_binding_learning(row: &rusqlite::Row<'_>) -> Result<BindingLearning> {
    let days = match row.get::<_, Option<String>>("days_json")? {
        None => BTreeSet::new(),
        Some(json) => {
            let values: Vec<String> = serde_json::from_str(&json)?;
            let set: BTreeSet<_> = values.iter().cloned().collect();
            if set.len() != values.len()
                || values.iter().any(|value| !valid_local_day(value))
                || values.iter().ne(set.iter())
            {
                return Err(Error::InvalidRoutingState(
                    "stored learned days are not a canonical sorted set".into(),
                ));
            }
            set
        }
    };
    let max_prompt: Option<i64> = row.get("max_prompt")?;
    if max_prompt.is_some_and(|value: i64| value < 0) {
        return Err(Error::InvalidRoutingState(
            "stored learned max prompt is negative".into(),
        ));
    }
    Ok(BindingLearning {
        binding_id: parse_id(
            "learned_models_by_binding.binding_id",
            row.get("binding_id")?,
        )?,
        model_id: row.get("model_id")?,
        days,
        max_prompt,
        explicitly_promoted: row.get("explicitly_promoted")?,
        updated_ms: row.get("updated_ms")?,
    })
}

pub(super) fn upsert_model(conn: &Connection, entry: &ModelEntry) -> Result<()> {
    let days_json = super::opt_json_to_text(&entry.days_json)?;
    let context_window_json = super::opt_json_to_text(&entry.context_window_json)?;
    conn.execute(
        "INSERT INTO models (model_id, days_json, max_prompt, context_window_json)
         VALUES (:model_id, :days_json, :max_prompt, :context_window_json)
         ON CONFLICT (model_id) DO UPDATE SET
             days_json = excluded.days_json,
             max_prompt = excluded.max_prompt,
             context_window_json = excluded.context_window_json",
        rusqlite::named_params! {
            ":model_id": entry.model_id,
            ":days_json": days_json,
            ":max_prompt": entry.max_prompt,
            ":context_window_json": context_window_json,
        },
    )?;
    Ok(())
}

pub(super) fn load_model(conn: &Connection, model_id: &str) -> Result<Option<ModelEntry>> {
    row_of(
        conn,
        "SELECT model_id, days_json, max_prompt, context_window_json
         FROM models WHERE model_id = ?1",
        [model_id],
        read_model,
    )
}

pub(super) fn load_models(conn: &Connection) -> Result<Vec<ModelEntry>> {
    rows_of(
        conn,
        "SELECT model_id, days_json, max_prompt, context_window_json
         FROM models ORDER BY model_id",
        [],
        read_model,
    )
}

fn read_model(row: &rusqlite::Row<'_>) -> Result<ModelEntry> {
    Ok(ModelEntry {
        model_id: row.get("model_id")?,
        days_json: super::opt_json_from_text(row.get("days_json")?)?,
        max_prompt: row.get("max_prompt")?,
        context_window_json: super::opt_json_from_text(row.get("context_window_json")?)?,
    })
}

/// Age and cap the lanes table: drop
/// lanes whose `updated_ms` sits in the future (a clock that moved — a
/// lane that could never go idle) or further back than `max_age_ms`, then
/// keep only the newest `max_lanes`. Rows the caller wrote are already
/// well-formed, so the file-era paranoia about hand-edited records does
/// not apply — the SQL is the whole rule. Returns how many rows went.
///
/// The caller owns the cadence: the prune runs on the 30-second flush,
/// never per request.
pub(super) fn prune_lanes(
    conn: &Connection,
    now_ms: i64,
    max_lanes: usize,
    max_age_ms: i64,
) -> Result<u64> {
    let aged = conn.execute(
        "DELETE FROM lanes WHERE updated_ms > ?1 OR updated_ms < ?2",
        [now_ms, now_ms.saturating_sub(max_age_ms)],
    )?;
    let capped = conn.execute(
        "DELETE FROM lanes WHERE key NOT IN
            (SELECT key FROM lanes ORDER BY updated_ms DESC LIMIT ?1)",
        [max_lanes.min(i64::MAX as usize) as i64],
    )?;
    Ok((aged + capped) as u64)
}

/// Idempotent: recording the same (session, meter, reset) allowance twice is
/// one row. A second release for the same window only ever widens it: an
/// overage grant over a plan one makes it overage, and a plan grant over
/// an overage one leaves it overage, because the burn marker already said
/// overage was fine for this window.
pub(super) fn record_allowance(conn: &Connection, allowance: &Allowance) -> Result<()> {
    conn.execute(
        "INSERT INTO allowances (session_id, account_number, meter, reset_value, release)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (session_id, account_number, meter, reset_value) DO UPDATE SET
           release = CASE WHEN excluded.release = 'overage' THEN 'overage'
                          ELSE allowances.release END",
        (
            &allowance.session_id,
            allowance.account_number,
            &allowance.meter,
            allowance.reset_value,
            release_name(allowance.release),
        ),
    )?;
    Ok(())
}

pub(super) fn load_allowances(conn: &Connection) -> Result<Vec<Allowance>> {
    rows_of(
        conn,
        "SELECT session_id, account_number, meter, reset_value, release FROM allowances
         ORDER BY session_id, account_number, meter, reset_value",
        [],
        read_allowance,
    )
}

/// One session's allowances, ordered by meter and reset value: the gate's
/// per-request read. The primary key leads with `session_id`, so this is
/// an index range rather than the whole table, which is what lets the
/// table carry other sessions' rows until the prune takes them.
pub(super) fn load_session_allowances(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<Allowance>> {
    rows_of(
        conn,
        "SELECT session_id, account_number, meter, reset_value, release FROM allowances
         WHERE session_id = ?1
         ORDER BY account_number, meter, reset_value",
        [session_id],
        read_allowance,
    )
}

/// Drop every allowance whose window has ended: its reset (epoch seconds)
/// is at or before `now_ms`. That is exactly the gate's own expiry rule
/// (`quota::expired`), so a pruned row is one the gate could no longer
/// honour: an allowance un-gates a meter only when its reset equals the
/// meter's reported reset, and a meter whose reset has passed is not
/// exhausted, whatever its utilisation. No slack, as in ctp's
/// `pruneAllowances`: a later reading of the same window cannot bring
/// the row back to life, because that window's reset is already past.
///
/// The column is `NOT NULL` and a grant is only recorded for a reported
/// reset, so there is no unknown-reset row to keep; a reset far in the
/// future is kept, which is the safe way to be wrong. Returns how many
/// rows went.
pub(super) fn prune_allowances(conn: &Connection, now_ms: i64) -> Result<u64> {
    let gone = conn.execute(
        "DELETE FROM allowances WHERE reset_value <= ?1",
        [now_ms.div_euclid(1000)],
    )?;
    Ok(gone as u64)
}

/// Delete every allowance `session_id` holds, live or not: closing a
/// session's gate. The ledger records the change as its own row; this
/// table is state, not history, so a delete here loses nothing the
/// ledger needs. Returns how many rows went.
pub(super) fn delete_session_allowances(conn: &Connection, session_id: &str) -> Result<u64> {
    let gone = conn.execute("DELETE FROM allowances WHERE session_id = ?1", [session_id])?;
    Ok(gone as u64)
}

fn read_allowance(row: &rusqlite::Row<'_>) -> Result<Allowance> {
    let release: String = row.get("release")?;
    Ok(Allowance {
        session_id: row.get("session_id")?,
        account_number: row.get("account_number")?,
        meter: row.get("meter")?,
        reset_value: row.get("reset_value")?,
        // A spelling this binary does not know reads as the narrower
        // release: a plan allowance can never spend overage.
        release: if release == "overage" {
            Release::Overage
        } else {
            Release::Plan
        },
    })
}

pub(super) fn record_ping(conn: &Connection, ping: &PingRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO pings (ts_ms, exit_code, duration_ms, boundary_ms,
                            slot, action, observed_ms, verified, assumed,
                            account_number)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        (
            ping.ts_ms,
            ping.exit_code,
            ping.duration_ms,
            ping.boundary_ms,
            ping.slot.as_deref(),
            ping.action.map(PingAction::as_str),
            ping.observed_ms,
            ping.verified,
            ping.assumed,
            ping.account_number,
        ),
    )?;
    Ok(())
}

/// Pings with `ts_ms >= ts_ms`, oldest first; like the ledger window, the
/// newest `limit` rows are kept when the window overflows.
pub(super) fn pings_since(conn: &Connection, ts_ms: i64, limit: i64) -> Result<Vec<PingRecord>> {
    rows_of(
        conn,
        "SELECT * FROM (
            SELECT * FROM pings WHERE ts_ms >= ?1
            ORDER BY ts_ms DESC, id DESC LIMIT ?2
        ) ORDER BY ts_ms ASC, id ASC",
        [ts_ms, limit],
        read_ping,
    )
}

fn read_ping(row: &rusqlite::Row<'_>) -> Result<PingRecord> {
    Ok(PingRecord {
        id: row.get("id")?,
        ts_ms: row.get("ts_ms")?,
        exit_code: row.get("exit_code")?,
        duration_ms: row.get("duration_ms")?,
        boundary_ms: row.get("boundary_ms")?,
        slot: row.get("slot")?,
        action: match row.get::<_, Option<String>>("action")? {
            None => None,
            Some(text) => Some(PingAction::parse(&text).ok_or(Error::UnknownDbValue {
                column: "action",
                value: text,
            })?),
        },
        observed_ms: row.get("observed_ms")?,
        verified: row.get("verified")?,
        assumed: row.get("assumed")?,
        account_number: row.get("account_number")?,
    })
}

/// A binding that refused the server safeguards ask, and when (spec:
/// WSAFE#a-rejection-mark).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeguardsMark {
    pub binding_id: BindingId,
    pub marked_ms: i64,
}

/// Mark a binding, keeping the first time it was marked.
pub(super) fn set_safeguards_mark(
    conn: &Connection,
    binding_id: &BindingId,
    now_ms: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO binding_safeguards_marks (binding_id, marked_ms) VALUES (?1, ?2)
         ON CONFLICT (binding_id) DO NOTHING",
        (binding_id.as_str(), now_ms),
    )?;
    Ok(())
}

/// Clear the marks held against `bindings`; how many were held.
pub(super) fn clear_safeguards_marks(conn: &Connection, bindings: &[&BindingId]) -> Result<u64> {
    let mut cleared = 0;
    for binding in bindings {
        cleared += conn.execute(
            "DELETE FROM binding_safeguards_marks WHERE binding_id = ?1",
            [binding.as_str()],
        )? as u64;
    }
    Ok(cleared)
}

pub(super) fn load_safeguards_marks(conn: &Connection) -> Result<Vec<SafeguardsMark>> {
    rows_of(
        conn,
        "SELECT binding_id, marked_ms FROM binding_safeguards_marks ORDER BY binding_id",
        [],
        |row| {
            Ok(SafeguardsMark {
                binding_id: parse_id("binding_safeguards_marks.binding_id", row.get(0)?)?,
                marked_ms: row.get(1)?,
            })
        },
    )
}

pub(super) fn save_meters(
    conn: &Connection,
    provider_id: &str,
    snapshot: &MetersSnapshot,
) -> Result<()> {
    let snapshot_json = serde_json::to_string(&snapshot.snapshot)?;
    conn.execute(
        "INSERT INTO meters_by_provider (provider_id, updated_ms, snapshot) VALUES (?1, ?2, ?3)
         ON CONFLICT (provider_id) DO UPDATE SET
             updated_ms = excluded.updated_ms,
             snapshot = excluded.snapshot",
        (provider_id, snapshot.updated_ms, snapshot_json),
    )?;
    Ok(())
}

pub(super) fn load_meters(conn: &Connection, provider_id: &str) -> Result<Option<MetersSnapshot>> {
    row_of(
        conn,
        "SELECT updated_ms, snapshot FROM meters_by_provider WHERE provider_id = ?1",
        [provider_id],
        read_meters,
    )
}

/// One account's latest meter snapshot on a provider. A reading belongs to
/// the account that reported it, so one account's never replaces another's
/// (spec: ACCT#what-is-attributed-to-an-account).
pub(super) fn save_account_meters(
    conn: &Connection,
    provider_id: &str,
    account_number: i64,
    snapshot: &MetersSnapshot,
) -> Result<()> {
    let snapshot_json = serde_json::to_string(&snapshot.snapshot)?;
    conn.execute(
        "INSERT INTO meters_by_account (provider_id, account_number, updated_ms, snapshot)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (provider_id, account_number) DO UPDATE SET
             updated_ms = excluded.updated_ms,
             snapshot = excluded.snapshot",
        (
            provider_id,
            account_number,
            snapshot.updated_ms,
            snapshot_json,
        ),
    )?;
    Ok(())
}

pub(super) fn load_account_meters(
    conn: &Connection,
    provider_id: &str,
    account_number: i64,
) -> Result<Option<MetersSnapshot>> {
    row_of(
        conn,
        "SELECT updated_ms, snapshot FROM meters_by_account
         WHERE provider_id = ?1 AND account_number = ?2",
        (provider_id, account_number),
        read_meters,
    )
}

/// Every account's latest snapshot on a provider, by account number: the
/// dashboard's strip reads them together to lay them out side by side.
pub(super) fn load_meters_by_account(
    conn: &Connection,
    provider_id: &str,
) -> Result<Vec<(i64, MetersSnapshot)>> {
    rows_of(
        conn,
        "SELECT account_number, updated_ms, snapshot FROM meters_by_account
         WHERE provider_id = ?1 ORDER BY account_number",
        [provider_id],
        |row| Ok((row.get("account_number")?, read_meters(row)?)),
    )
}

fn read_meters(row: &rusqlite::Row<'_>) -> Result<MetersSnapshot> {
    let snapshot = super::opt_json_from_text(row.get::<_, Option<String>>("snapshot")?)?
        .expect("meters_state.snapshot is NOT NULL");
    Ok(MetersSnapshot {
        updated_ms: row.get("updated_ms")?,
        snapshot,
    })
}

pub(super) fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        (key, value),
    )?;
    Ok(())
}

pub(super) fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    row_of(
        conn,
        "SELECT value FROM meta WHERE key = ?1",
        [key],
        |row| Ok(row.get(0)?),
    )
}

/// The local calendar day of an epoch-millisecond timestamp, `YYYY-MM-DD`
/// (the same shape an `en-CA` locale day renders as). The timezone is the
/// caller's, so the grouping is a pure function of its inputs; the server
/// passes the system zone, and tests pin theirs.
pub fn local_day(at_ms: i64, tz: &jiff::tz::TimeZone) -> Option<String> {
    let zoned = jiff::Timestamp::from_millisecond(at_ms)
        .ok()?
        .to_zoned(tz.clone());
    Some(zoned.date().to_string())
}

#[cfg(test)]
mod tests {
    use jiff::tz::TimeZone;

    use super::local_day;

    #[test]
    fn days_group_by_local_calendar_day_never_utc() {
        let tz = TimeZone::fixed(jiff::tz::Offset::from_hours(13).expect("offset"));
        // 2026-02-01 14:00 UTC is 2026-02-02 03:00 at +13: the same instant
        // on a different local day, and the LOCAL one is what counts.
        let utc_day = local_day(1_769_954_400_000, &TimeZone::UTC).expect("parses");
        assert_eq!(utc_day, "2026-02-01");
        let local = local_day(1_769_954_400_000, &tz).expect("parses");
        assert_eq!(local, "2026-02-02", "+13:00 is already the next day");

        // A timestamp that cannot exist answers None, never a guess.
        assert_eq!(local_day(i64::MAX, &tz), None);
    }
}

/// The window the routing page counts and replays over: the last 24 hours
/// (spec: UTUI#reading-a-table).
pub const ROUTING_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// What a window's rows can say about which rule caught a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitCoverage {
    /// Every policy-routed row in the window recorded what matched.
    Complete,
    /// Some did and some predate it, so each count is at least that many.
    Floor,
    /// None did. The counts are empty because nothing was recorded, not
    /// because nothing matched.
    NotRecorded,
}

/// How many requests a default caught, by the scope it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultHits {
    pub scope: FrontendScope,
    pub hits: u64,
}

/// The last 24 hours' requests counted by the record that decided their
/// route, by the record's identifier across every revision the window
/// spans (spec: UTUI#reading-a-table). A request is its first attempt, or its
/// terminal where nothing was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitCounts {
    pub since_ms: i64,
    pub until_ms: i64,
    pub coverage: HitCoverage,
    /// Requests whose rows recorded what matched.
    pub recorded_requests: u64,
    /// Requests from before that was recorded. They are in no count below.
    pub unrecorded_requests: u64,
    pub presentations: BTreeMap<PresentationId, u64>,
    pub rewrites: BTreeMap<RewriteId, u64>,
    pub defaults: Vec<DefaultHits>,
    /// Caught by the compaction policy's own route rather than by a rule.
    pub compactions: u64,
    /// Recorded requests no lookup matched, or whose lane's pin refused
    /// before a route was chosen.
    pub unrouted: u64,
}

/// Count the 24 hours ending at `until_ms`. One indexed range scan of
/// `requests` on `ts_ms`.
///
/// A row from before the facts were recorded carries its presentation or
/// rewrite all the same, so those two counts take it; only what the lookup
/// columns say, the default that matched and the scope it matched in, is
/// never inferred for such a row. A row whose stored identifier does not
/// parse is counted as unrouted rather than taking the whole page down with
/// it, as a row whose kind does not parse already was.
pub(super) fn hit_counts(conn: &Connection, until_ms: i64) -> Result<HitCounts> {
    let since_ms = until_ms - ROUTING_WINDOW_MS;
    let rows = rows_of(
        conn,
        "SELECT facts_recorded, lookup_kind, lookup_scope, presentation_id, rewrite_id,
                facts_profile, facts_protocol, COUNT(*) AS hits
         FROM requests
         WHERE ts_ms >= ?1 AND ts_ms <= ?2 AND policy_revision_id IS NOT NULL
           AND (attempt_index = 0 OR attempt_index IS NULL)
         GROUP BY facts_recorded, lookup_kind, lookup_scope, presentation_id,
                  rewrite_id, facts_profile, facts_protocol",
        [since_ms, until_ms],
        |row| {
            Ok((
                row.get::<_, Option<i64>>("facts_recorded")?,
                row.get::<_, Option<String>>("lookup_kind")?,
                row.get::<_, Option<String>>("lookup_scope")?,
                row.get::<_, Option<String>>("presentation_id")?,
                row.get::<_, Option<String>>("rewrite_id")?,
                row.get::<_, Option<String>>("facts_profile")?,
                row.get::<_, Option<String>>("facts_protocol")?,
                u64::try_from(row.get::<_, i64>("hits")?).unwrap_or(0),
            ))
        },
    )?;
    let mut counts = HitCounts {
        since_ms,
        until_ms,
        coverage: HitCoverage::Complete,
        recorded_requests: 0,
        unrecorded_requests: 0,
        presentations: BTreeMap::new(),
        rewrites: BTreeMap::new(),
        defaults: Vec::new(),
        compactions: 0,
        unrouted: 0,
    };
    let mut defaults: BTreeMap<String, DefaultHits> = BTreeMap::new();
    for (marker, kind, scope, presentation, rewrite, profile, protocol, hits) in rows {
        if marker.is_none() {
            counts.unrecorded_requests += hits;
            // The row still names the presentation or rewrite that matched,
            // so those counts take it; it says nothing about a default.
            match (
                presentation.and_then(|id| id.parse::<PresentationId>().ok()),
                rewrite.and_then(|id| id.parse::<RewriteId>().ok()),
            ) {
                (Some(id), _) => *counts.presentations.entry(id).or_default() += hits,
                (None, Some(id)) => *counts.rewrites.entry(id).or_default() += hits,
                (None, None) => {}
            }
            continue;
        }
        counts.recorded_requests += hits;
        let kind = parse_required("requests.lookup_kind", kind.unwrap_or_default(), |value| {
            super::ledger::LookupKind::parse(value)
        });
        let Ok(kind) = kind else {
            counts.unrouted += hits;
            continue;
        };
        match kind {
            super::ledger::LookupKind::Compaction => counts.compactions += hits,
            super::ledger::LookupKind::Presentation => {
                match presentation.and_then(|id| id.parse::<PresentationId>().ok()) {
                    Some(id) => *counts.presentations.entry(id).or_default() += hits,
                    None => counts.unrouted += hits,
                }
            }
            super::ledger::LookupKind::Rewrite => {
                match rewrite.and_then(|id| id.parse::<RewriteId>().ok()) {
                    Some(id) => *counts.rewrites.entry(id).or_default() += hits,
                    None => counts.unrouted += hits,
                }
            }
            super::ledger::LookupKind::Default => {
                let protocol = parse_required(
                    "requests.facts_protocol",
                    protocol.unwrap_or_default(),
                    crate::policy::facts::parse_protocol,
                )?;
                let scope = parse_required(
                    "requests.lookup_scope",
                    scope.unwrap_or_default(),
                    super::ledger::LookupScope::parse,
                )?;
                let scope = match scope {
                    super::ledger::LookupScope::Protocol => FrontendScope::Protocol { protocol },
                    super::ledger::LookupScope::Profile => FrontendScope::Profile {
                        profile: parse_id("requests.facts_profile", profile.unwrap_or_default())?,
                        protocol,
                    },
                };
                let key = serde_json::to_string(&scope)
                    .map_err(|error| Error::InvalidRoutingState(error.to_string()))?;
                defaults
                    .entry(key)
                    .or_insert(DefaultHits { scope, hits: 0 })
                    .hits += hits;
            }
        }
    }
    counts.defaults = defaults.into_values().collect();
    counts.coverage = match (counts.unrecorded_requests, counts.recorded_requests) {
        (0, _) => HitCoverage::Complete,
        (_, 0) => HitCoverage::NotRecorded,
        _ => HitCoverage::Floor,
    };
    Ok(counts)
}

/// One recent policy-routed request, with the facts a preview needs where its
/// row recorded them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentRequest {
    pub id: i64,
    pub ts_ms: i64,
    pub policy_revision_id: i64,
    /// Present on a row of nothing sent (`route-unavailable`).
    pub kind: Option<String>,
    /// What the row says the request was, for a row that predates the facts
    /// to be shown by.
    pub requested_model: Option<String>,
    /// How it routed at the time.
    pub route_id: Option<String>,
    pub candidate_id: Option<String>,
    pub binding_id: Option<String>,
    pub effective_model: Option<String>,
    /// The record that decided the route, as recorded: its kind, scope and
    /// identifier. Absent on a row from before the lookup was recorded.
    pub matched: Option<RecentMatch>,
    /// The facts the resolver was given. Absent on a row from before they
    /// were recorded, which cannot be replayed.
    pub facts: Option<RecordedFacts>,
}

impl RecentRequest {
    /// Whether the request can be resolved again without a fact assumed.
    pub fn replayable(&self) -> bool {
        self.facts.as_ref().is_some_and(RecordedFacts::replayable)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentMatch {
    pub kind: super::ledger::LookupKind,
    pub scope: Option<super::ledger::LookupScope>,
    pub id: Option<String>,
}

const RECENT_REQUEST_COLUMNS: &str = "id, ts_ms, policy_revision_id, kind, requested_model,
    policy_route_id, candidate_id, binding_id, effective_model,
    presentation_id, rewrite_id, lookup_kind, lookup_scope,
    facts_recorded, facts_profile, facts_protocol, facts_requested_model,
    facts_prompt_bound, facts_message_count, facts_tool_count, facts_capabilities,
    facts_asks_safeguards, facts_binding_affinity, facts_class,
    facts_lane, facts_lane_route_fingerprint, facts_lane_candidate_id,
    facts_lane_binding_id, facts_lane_effective_model,
    facts_lane_forced_from, facts_lane_forced_to";

fn read_recent_request(row: &rusqlite::Row<'_>) -> Result<RecentRequest> {
    let matched = match row.get::<_, Option<String>>("lookup_kind")? {
        None => None,
        Some(kind) => Some(RecentMatch {
            kind: parse_required("requests.lookup_kind", kind, |value| {
                super::ledger::LookupKind::parse(value)
            })?,
            scope: row
                .get::<_, Option<String>>("lookup_scope")?
                .map(|scope| {
                    parse_required(
                        "requests.lookup_scope",
                        scope,
                        super::ledger::LookupScope::parse,
                    )
                })
                .transpose()?,
            id: row
                .get::<_, Option<String>>("presentation_id")?
                .or(row.get::<_, Option<String>>("rewrite_id")?),
        }),
    };
    Ok(RecentRequest {
        id: row.get("id")?,
        ts_ms: row.get("ts_ms")?,
        policy_revision_id: row.get("policy_revision_id")?,
        kind: row.get("kind")?,
        requested_model: row.get("requested_model")?,
        route_id: row.get("policy_route_id")?,
        candidate_id: row.get("candidate_id")?,
        binding_id: row.get("binding_id")?,
        effective_model: row.get("effective_model")?,
        matched,
        facts: super::ledger::read_recorded_facts(row)?,
    })
}

/// The newest policy-routed requests since `since_ms`, newest first, at most
/// `limit`. A request is its first attempt, or its terminal where nothing was
/// sent, as the hit counts take it.
pub(super) fn recent_requests(
    conn: &Connection,
    since_ms: i64,
    limit: u32,
) -> Result<Vec<RecentRequest>> {
    rows_of(
        conn,
        &format!(
            "SELECT {RECENT_REQUEST_COLUMNS} FROM requests
             WHERE ts_ms >= ?1 AND policy_revision_id IS NOT NULL
               AND (attempt_index = 0 OR attempt_index IS NULL)
             ORDER BY ts_ms DESC, id DESC LIMIT ?2"
        ),
        rusqlite::params![since_ms, limit],
        read_recent_request,
    )
}

/// One policy-routed request by its row id, where it is one.
pub(super) fn recent_request(conn: &Connection, id: i64) -> Result<Option<RecentRequest>> {
    row_of(
        conn,
        &format!(
            "SELECT {RECENT_REQUEST_COLUMNS} FROM requests
             WHERE id = ?1 AND policy_revision_id IS NOT NULL
               AND (attempt_index = 0 OR attempt_index IS NULL)"
        ),
        [id],
        read_recent_request,
    )
}
