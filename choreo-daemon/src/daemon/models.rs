//! Per-account model-list handling: the model cache, the detached background
//! prefetch, and the `ListModels` / `ValidateModel` handlers.
//!
//! These are the `impl DaemonState` methods that keep an account's provider
//! model list warm, plus the free-function helper that seeds a cold cache from
//! the local catalog. They live in a child module (same pattern as
//! `daemon/image_provider.rs` and `daemon/session_image.rs`) so `daemon.rs`
//! stays focused on core command handling; the items are `pub(super)` because
//! the `DaemonCommand` arms in the parent dispatch to them.
//!
//! As a CHILD of `crate::daemon`, this module reaches the parent's private
//! items (`DaemonState` fields, its provider building, its broadcast fan-out)
//! through `super`.
//!
//! Three rules hold across the whole module:
//!
//! - **The command loop is the single writer of `DaemonState::model_cache`.**
//!   A prefetch thread never touches the cache: it reports its outcome back as
//!   [`DaemonCommand::ModelPrefetchResult`] and the loop performs the insert
//!   ([`DaemonState::handle_model_prefetch_result`]). That keeps cache
//!   mutations serialized with every other daemon-state change.
//! - **The prefetch is detached and dedup-guarded.** Fetching a model list is a
//!   network round-trip bounded by the provider request timeout (retried), so
//!   it runs on its own thread and never on the command loop.
//!   `model_prefetch_in_flight` holds one entry per account with a fetch
//!   running, so a burst of session joins (or an account switch per request)
//!   cannot stack duplicate HTTP fetches for the same account.
//! - **`ListModels` never blocks.** [`DaemonState::handle_list_models`] answers
//!   from the cache, from the local catalog's curated ids, or with an empty
//!   PROVISIONAL list when it has neither ("nothing to paint, a fetch is
//!   running") — it only *triggers* a background prefetch, whose landing is
//!   pushed to clients as `ModelsUpdated` (or `ModelsUnavailable` on failure)
//!   and replaces whatever an already-open picker painted.
//!
//! `ValidateModel` is the fail-open consumer of the same cache: a model is
//! rejected only when a cached list exists and does not contain it, because a
//! list we could not fetch is no evidence that the model is invalid.

use super::{
    DaemonCommand, DaemonMessageType, DaemonState, ListModelsResult, ModelReasoningResult,
};
use choreo_ai_protocols::{lookup_provider, model_reasoning_capability};
use choreo_proto::{ModelInfo, ReasoningCapability};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// TTL for cached provider model lists. Shared by the freshness checks in
/// [`DaemonState::handle_list_models`] and the background-prefetch guard
/// ([`DaemonState::should_prefetch_models`]) so both paths agree on what
/// "fresh" means.
pub(super) const MODEL_CACHE_TTL: Duration = Duration::from_secs(300);

/// Case-insensitive lookup of `id` among model ids.
///
/// The single membership rule shared by the suggestion path and model
/// validation. It has to be case-insensitive because the daemon orders ids
/// case-insensitively (`sort_model_ids`) while keeping the provider's casing, so
/// an exact `==` would call a differently-cased id "unsupported" and reject a
/// model the list plainly contains. Returns the list's own spelling of the id
/// so a caller that wants to hand the id back does not echo the caller's casing.
pub(super) fn match_model_id<'a>(
    ids: impl IntoIterator<Item = &'a str>,
    id: &str,
) -> Option<&'a str> {
    ids.into_iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(id))
}

/// The model a client should pre-highlight for a pending choice.
///
/// Preference order: the current selection when the list serves it (so
/// re-opening the picker lands on the active row), else the provider catalog's
/// default model when the list serves that (so a first pick lands somewhere
/// the provider itself recommends), else the first id. `None` only for an empty
/// list — a list with any row always has a sensible pre-selection.
pub(super) fn suggest_model(
    current: Option<&str>,
    list: &[ModelInfo],
    catalog_default: Option<&str>,
) -> Option<String> {
    let ids = || list.iter().map(|model| model.id.as_str());
    if let Some(current) = current
        && let Some(id) = match_model_id(ids(), current)
    {
        return Some(id.to_string());
    }
    if let Some(default) = catalog_default
        && let Some(id) = match_model_id(ids(), default)
    {
        return Some(id.to_string());
    }
    list.first().map(|model| model.id.clone())
}

/// The reasoning effort a client should pre-select for a pending choice.
///
/// Preference order: the current effort when the model supports it (including
/// an explicit `"off"` — a user who turned reasoning off stays off), else the
/// middle supported level. `None` only when the model has no levels at all,
/// where the only valid effort is `"off"`.
pub(super) fn suggest_effort(current: Option<&str>, levels: &[String]) -> Option<String> {
    if levels.is_empty() {
        return None;
    }
    if let Some(current) = current
        && levels.iter().any(|level| level == current)
    {
        return Some(current.to_string());
    }
    ReasoningCapability {
        available_effort_levels: levels.to_vec(),
    }
    .middle()
    .map(str::to_string)
}

/// Attach one id's reasoning levels: a pure catalog lookup keyed by the
/// account's provider slug. `slug: None` (an account whose provider the daemon
/// cannot resolve) yields an empty level set, which every consumer reads as
/// "this model does not support reasoning".
fn model_info(slug: Option<&str>, id: String) -> ModelInfo {
    let reasoning_levels = slug.map_or_else(Vec::new, |slug| {
        model_reasoning_capability(slug, &id).available_effort_levels
    });
    ModelInfo {
        id,
        reasoning_levels,
    }
}

/// Attach reasoning levels to a whole id list, preserving its order.
fn model_infos(slug: Option<&str>, ids: &[String]) -> Vec<ModelInfo> {
    ids.iter().map(|id| model_info(slug, id.clone())).collect()
}

/// Put model ids in the one order every surface presents: case-insensitively by
/// id. A provider's `/v1/models` returns whatever order it likes, and the local
/// catalog's curated set has its own; sorting at the two points where a list
/// enters the daemon (a fetched list and the catalog seed) makes the cache, the
/// `Models` reply, the `ModelsUpdated` broadcast and `/model`'s listing agree
/// without each frontend imposing its own order.
///
/// `sort_by_cached_key` computes the lowercase key once per element instead of
/// once per comparison, and is stable, so ids differing only by case keep their
/// relative order.
pub(super) fn sort_model_ids(models: &mut [String]) {
    models.sort_by_cached_key(|model| model.to_lowercase());
}

impl DaemonState {
    /// Decide whether the model list for `account` needs a background
    /// prefetch: only when the account is configured AND holds a decrypted
    /// credential (otherwise the client build would fail anyway), no fetch is
    /// already running for the account, and the cached list is missing or
    /// past [`MODEL_CACHE_TTL`].  Pure — no side effects — so tests can
    /// assert the gate independently of thread spawning.
    pub(super) fn should_prefetch_models(&self, account: &str) -> bool {
        if !self.accounts.contains(account)
            || !self.has_credential(account)
            || self.model_prefetch_in_flight.contains(account)
        {
            return false;
        }
        // Prefetch unless a verified (fresh, non-empty) list is already cached.
        // An empty entry is treated as absent so this gate agrees with
        // `handle_list_models`, which paints a catalog seed over an empty cache.
        self.fresh_model_list(account, Instant::now()).is_none()
    }

    /// The account's cached model list WHEN it is the verified one: present,
    /// NON-EMPTY, and within [`MODEL_CACHE_TTL`]. An empty entry is treated as
    /// absent (it paints nothing), so the prefetch gate
    /// ([`Self::should_prefetch_models`]) and the on-demand path
    /// ([`Self::handle_list_models`]) agree that an empty or stale cache needs a
    /// fresh fetch. Borrows the cached vector; a caller that must hold it across
    /// a `&mut self` call clones first.
    fn fresh_model_list(&self, account: &str, now: Instant) -> Option<&Vec<String>> {
        match self.model_cache.get(account) {
            Some((models, cached_at))
                if !models.is_empty() && now.duration_since(*cached_at) < MODEL_CACHE_TTL =>
            {
                Some(models)
            }
            _ => None,
        }
    }

    /// Spawn a detached background thread that fetches the model list for
    /// `account` and reports the outcome back to the command loop via
    /// [`DaemonCommand::ModelPrefetchResult`] — the loop, not the fetch
    /// thread, owns `model_cache` and the in-flight guard.  A no-op unless
    /// [`Self::should_prefetch_models`] says a fetch is needed, which is what
    /// keeps a burst of session joins (or an account switch per request) from
    /// stacking duplicate HTTP fetches.  A failed spawn releases the guard so
    /// the account stays re-prefetchable.
    pub(super) fn maybe_spawn_model_prefetch(&mut self, account: &str) {
        if !self.should_prefetch_models(account) {
            return;
        }
        self.model_prefetch_in_flight.insert(account.to_string());
        // Build the client fresh against the daemon registry: there is no
        // provider cache anymore. `should_prefetch_models` guarantees config
        // + credential exist; the None arm is belt-and-braces so the
        // in-flight guard can never leak.
        let Some(provider) = self.build_daemon_provider(account) else {
            self.model_prefetch_in_flight.remove(account);
            return;
        };
        let daemon_tx = self.daemon_tx.clone();
        let account_name = account.to_string();
        let spawned = thread::Builder::new()
            .name(format!("model-prefetch-{account_name}"))
            .spawn(move || {
                // The fetch is deliberately detached from the command loop:
                // a slow provider endpoint (up to the full request timeout,
                // retried) must never stall daemon commands the way the old
                // unlock-time synchronous prefetch did.
                //
                // The whole fetch is wrapped in `catch_unwind` (the provider
                // is an owned value, so `AssertUnwindSafe` is sound here —
                // the thread never touches shared state): a panic inside the
                // provider's HTTP/serde code is not covered by the
                // workspace's no-panic discipline, and an uncaught unwind
                // would skip the `ModelPrefetchResult` send below — the ONLY
                // message that releases the in-flight guard — permanently
                // wedging the account against re-prefetching until daemon
                // restart. A caught panic is reported as a plain fetch
                // error, and the next join retries.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    provider.list_models().map_err(|e| e.to_string())
                }))
                .unwrap_or_else(|panic| {
                    // Panic payloads are `String`/`&str` in practice, but
                    // the payload type is `dyn Any` — fall back to a generic
                    // message rather than assuming the shape.
                    let detail = panic
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_string()))
                        .unwrap_or_else(|| "unknown panic payload".to_string());
                    Err(format!("model list fetch panicked: {detail}"))
                });
                let _ = daemon_tx.send(DaemonCommand::ModelPrefetchResult {
                    account: account_name,
                    result,
                });
            });
        if let Err(e) = spawned {
            self.model_prefetch_in_flight.remove(account);
            warn!(
                account = %account,
                error = %e,
                "failed to spawn model prefetch thread; account stays re-prefetchable"
            );
        }
    }

    /// Receive a background model-prefetch outcome: release the account's
    /// in-flight guard and, on success, populate `model_cache` (the command
    /// loop is its single writer).  A failure is pushed to clients as
    /// `ModelsUnavailable` so a picker waiting on this fetch learns it is over
    /// instead of waiting forever; the on-demand path in
    /// [`Self::handle_list_models`] keeps serving whatever it can in the
    /// meantime, and the next session join re-prefetches.
    pub(super) fn handle_model_prefetch_result(
        &mut self,
        account: String,
        result: Result<Vec<String>, String>,
    ) {
        self.model_prefetch_in_flight.remove(&account);
        match result {
            Ok(mut models) => {
                // Only cache while the account still exists with a credential:
                // the account may have been removed or reconfigured
                // (AccountsReload invalidates sessions) while the fetch was
                // in flight, and inserting then would serve a dead
                // provider's model list for a full TTL.
                if !self.accounts.contains(&account) || !self.has_credential(&account) {
                    debug!(
                        account = %account,
                        "discarding background model prefetch result; \
                         account was removed or reconfigured while the fetch ran"
                    );
                    return;
                }
                debug!(
                    account = %account,
                    models = models.len(),
                    "background model prefetch complete"
                );
                // One order for every consumer: the cache, the `Models` reply,
                // this broadcast and the `/model` listing all read this list, so
                // the ordering is applied once here rather than by each
                // frontend independently.
                sort_model_ids(&mut models);
                let now = Instant::now();
                // Skip the broadcast ONLY when the cached entry is already a
                // verified (fresh, non-empty) list that matches this fetch — the
                // clients already hold it. A stale or absent entry must be
                // re-announced even when the ids are identical: an open picker
                // is showing that entry as a provisional stand-in, and only the
                // broadcast clears the label (the picker keeps the user's row
                // across the swap, so re-announcing costs no cursor position).
                let already_verified = self
                    .fresh_model_list(&account, now)
                    .is_some_and(|cached| cached == &models);
                // Cache unconditionally: the insert refreshes the freshness
                // stamp `MODEL_CACHE_TTL` is measured against, so a prefetch
                // that returned the same list still postpones the next one.
                self.model_cache
                    .insert(account.clone(), (models.clone(), now));
                if already_verified {
                    debug!(
                        account = %account,
                        "background model prefetch matched the verified cache; \
                         freshness refreshed, no broadcast"
                    );
                    return;
                }
                // Announce the freshly fetched list so an already-open model
                // picker replaces its list (a catalog seed, or a stale cache
                // entry) in place instead of waiting for the user to reopen it.
                // The broadcast is keyed by account name; a frontend applies it
                // only to the picker showing that account. It rides the ACTIVITY
                // bus only: the model list is not session-list state, so a
                // summary subscriber has nothing to do with it.
                //
                // The broadcast carries each id's reasoning levels, so a picker
                // that was painted from a provisional seed never loses the
                // levels it needs to offer an effort along with the model.
                let slug = self.account_provider_slug(&account);
                let models = model_infos(slug.as_deref(), &models);
                self.handle_broadcast_activity(
                    None,
                    &choreo_proto::DaemonMessage::broadcast(DaemonMessageType::ModelsUpdated {
                        account,
                        models,
                    }),
                );
            }
            Err(e) => {
                // A client may be showing a waiting row for exactly this
                // fetch (an empty provisional `Models` reply), so its failure
                // is announced rather than only logged. The text is the
                // provider's own message: it goes on the wire, where the
                // client displays it, but never into the log verbatim — only
                // its length, like every other payload field.
                warn!(
                    account = %account,
                    error_len = e.len(),
                    "background model prefetch failed; announcing ModelsUnavailable"
                );
                self.handle_broadcast_activity(
                    None,
                    &choreo_proto::DaemonMessage::broadcast(DaemonMessageType::ModelsUnavailable {
                        account,
                        error: e,
                    }),
                );
            }
        }
    }

    /// Validate that a model exists in the provider's model list for this
    /// session's account.  The model list is warmed by the background
    /// prefetch spawned at session join/attach/account-switch time
    /// (`maybe_spawn_model_prefetch`).  If no cached data exists (fetch
    /// failed or the prefetch hasn't landed yet) the model is allowed
    /// through — we'd rather fail at inference time than reject a potentially
    /// valid model we couldn't verify.
    pub(super) fn handle_validate_model(
        &mut self,
        session_id: u64,
        account: Option<&str>,
        model: &str,
        reply: &mpsc::Sender<Result<(), String>>,
    ) {
        debug!(
            "ValidateModel: session_id={}, explicit_account={}",
            session_id,
            account.is_some()
        );

        // An explicit account wins: the first-run flow validates a candidate
        // account's model before the session is bound to it, so the session's
        // own account would be the wrong list to check against.
        let account_name = if let Some(name) = account.map(str::to_string) {
            name
        } else {
            let Some(name) = self
                .session_metadata
                .get(&session_id)
                .and_then(|m| m.account_name.clone())
            else {
                debug!(
                    "ValidateModel: no session or no account attached, \
                     allowing model '{model}' through"
                );
                let _ = reply.send(Ok(()));
                return;
            };
            name
        };

        // No resolvable client for this account (locked, credential missing,
        // or account unknown) → reject so the user knows they must unlock
        // first (or configure a credential) rather than silently accepting an
        // unvalidated model.
        if !self.accounts.contains(&account_name) || !self.has_credential(&account_name) {
            debug!(
                "ValidateModel: no provider for account '{account_name}', \
                 rejecting model '{model}'"
            );
            let _ = reply.send(Err(format!(
                "daemon is locked or no credential configured for account \
                 '{account_name}'"
            )));
            return;
        }

        // Check the cache.  If missing (fetch failed earlier) or empty,
        // allow through rather than reject a potentially valid model.
        match self.model_cache.get(&account_name) {
            Some((cached_models, _cached_at)) if !cached_models.is_empty() => {
                // Case-insensitive membership: the cache keeps the provider's
                // casing while the id a client sends may differ in case. This
                // is the SAME rule the suggestion path uses, so a model the
                // picker suggests can never be rejected here.
                if match_model_id(cached_models.iter().map(String::as_str), model).is_some() {
                    let _ = reply.send(Ok(()));
                } else {
                    let available = humfmt::list(cached_models);
                    let _ = reply.send(Err(format!(
                        "model '{model}' not found. Available: {available}"
                    )));
                }
            }
            _ => {
                debug!(
                    "ValidateModel: no cached models for account '{account_name}', \
                     allowing model '{model}' through"
                );
                let _ = reply.send(Ok(()));
            }
        }
    }

    /// The `ListModels` handler: resolve the requested account (an explicit
    /// candidate account, else the session's), then answer with the best model
    /// list available WITHOUT ever performing a fetch on the command loop.
    ///
    /// A fresh, non-empty cache answers immediately; otherwise the fetch is
    /// handed to a detached background thread (dedup-guarded) and the reply is
    /// the stale cached list, the local catalog's curated ids, or — when neither
    /// exists — the retryable "warming" error from `choreo_proto`. The selected
    /// model rides along so a picker can mark the active row, and
    /// [`ListModelsResult::provisional`] tells the picker whether to present the
    /// ids as the account's live list or as a stand-in, so a seeded list is
    /// never mistaken for a verified one.
    pub(super) fn handle_list_models(
        &mut self,
        session_id: Option<u64>,
        requested_account: Option<&str>,
    ) -> Result<ListModelsResult, String> {
        // The session's account scopes the defaults (and the active-row marker);
        // an explicit candidate account overrides it before anything is bound.
        let session_account = session_id
            .and_then(|sid| self.session_metadata.get(&sid))
            .and_then(|m| m.account_name.clone());
        let account_name = requested_account
            .map(str::to_string)
            .or_else(|| session_account.clone())
            .unwrap_or_default();

        debug!(
            "ListModels: session_id={:?}, account_name='{}', explicit={}",
            session_id,
            account_name,
            requested_account.is_some()
        );

        // Existence + credential check only — no provider instance is needed
        // below, because the actual fetch (if any) runs on the detached
        // background thread.
        if !self.accounts.contains(&account_name) || !self.has_credential(&account_name) {
            return Err(if self.accounts.is_empty() {
                "no accounts configured".to_string()
            } else {
                format!("no credential stored for account '{account_name}'")
            });
        }

        // A fresh, non-empty cache is the verified list and answers immediately.
        // Otherwise the fetch NEVER runs here synchronously: a blocking HTTP
        // round-trip (up to the full request timeout, retried) would stall the
        // whole daemon command loop — the exact stall the background-prefetch
        // design removed from unlock. The request instead TRIGGERS a background
        // prefetch (dedup-guarded via `maybe_spawn_model_prefetch` — no-op when
        // one is already running, so an open picker while a join-time prefetch
        // is in flight does not double-fetch) and serves the best list it can
        // WITHOUT blocking:
        //   - a fresh, non-empty cache answers immediately (and is NOT
        //     provisional: the daemon verified it for this account);
        //   - else a stale-but-present list is served (it beats nothing);
        //   - else the local catalog's curated ids for the account's provider
        //     are served as an instant first paint, so the picker opens without
        //     waiting on a slow endpoint (api.openai.com's /v1/models can take
        //     ~10 s per attempt and trip a 504 retry);
        //   - only when none of those exist does it answer with an EMPTY list
        //     flagged `provisional` — "there is nothing to paint and a fetch is
        //     running" (or, if no fetch could even be started, a genuine
        //     error).
        // Both stand-ins report `provisional = true`. An EMPTY cached list is
        // treated as absent (see `fresh_model_list`) so the seed stands in for
        // it. When the background fetch settles it broadcasts `ModelsUpdated`
        // (or, on failure, `ModelsUnavailable`), which replaces whatever was
        // painted in an already-open picker — and, because a stale or absent
        // entry is always re-announced even when the ids are identical, an open
        // picker's provisional label clears once the live fetch confirms the
        // list. Those two pushes are what end the waiting state: a client packs
        // no timer for an answer the daemon promises to send.
        let now = Instant::now();
        let (models, provisional) =
            if let Some(models) = self.fresh_model_list(&account_name, now).cloned() {
                (models, false)
            } else {
                // Clone the stale list (if any) BEFORE the mutable spawn call,
                // so no borrow of `self` is live across it.
                let stale = self
                    .model_cache
                    .get(&account_name)
                    .map(|(models, _)| models.clone())
                    .filter(|models| !models.is_empty());
                // Paint from the local catalog only when there is no cached list
                // at all: a present (even stale) list is more account-specific
                // than the catalog's provider-wide curated set.
                let seed = if stale.is_none() {
                    self.catalog_models_for_account(&account_name)
                } else {
                    None
                };
                self.maybe_spawn_model_prefetch(&account_name);
                if let Some(models) = stale.or(seed) {
                    (models, true)
                } else {
                    // No cache, no stale entry, no catalog seed. Answer with an
                    // EMPTY provisional list ONLY when a fetch is actually
                    // running, because that list is the client's cue to show a
                    // waiting row: it ends when the fetch pushes
                    // `ModelsUpdated` or `ModelsUnavailable`. A prefetch that
                    // could not be spawned (no buildable provider for the
                    // account) has nothing behind it, so say so plainly instead
                    // of leaving the client waiting for a push that never comes.
                    if !self.model_prefetch_in_flight.contains(&account_name) {
                        return Err(format!(
                            "no model list available for account '{account_name}'"
                        ));
                    }
                    (Vec::new(), true)
                }
            };

        // The active-row marker is the session's own model, which says nothing
        // about a DIFFERENT account's list: only the account the session is
        // bound to can carry the model the session currently uses.
        let selected_model = session_id
            .filter(|_| session_account.as_deref() == Some(account_name.as_str()))
            .and_then(|sid| self.session_metadata.get(&sid))
            .and_then(|m| m.selected_model.clone());
        let slug = self.account_provider_slug(&account_name);
        let models = model_infos(slug.as_deref(), &models);
        // The provider's own default is the second suggestion preference, so a
        // first pick lands on a model the provider recommends rather than
        // merely the alphabetically-first one.
        let catalog_default = slug
            .as_deref()
            .and_then(lookup_provider)
            .map(|entry| entry.default_model);
        let suggested_model = suggest_model(
            selected_model.as_deref(),
            &models,
            catalog_default.as_deref(),
        );
        Ok(ListModelsResult {
            models,
            selected_model,
            suggested_model,
            provisional,
        })
    }

    /// The `ModelReasoning` handler: describe a candidate model's reasoning
    /// capability and the effort a client should pre-select for it.
    ///
    /// Pure and cheap — a catalog lookup plus a suggestion — because the
    /// first-run flow asks about every model it lists, and anything heavier
    /// (a provider instance, a network round-trip) would make the picker lag
    /// behind the keyboard.
    pub(super) fn handle_model_reasoning(
        &self,
        account: &str,
        model: &str,
        session_id: Option<u64>,
    ) -> ModelReasoningResult {
        // An unresolvable account has no provider to ask, so the model carries
        // an empty level set — the same shape a reasoning-free model has.
        let capability = self.account_provider_slug(account).map_or_else(
            || ReasoningCapability {
                available_effort_levels: Vec::new(),
            },
            |slug| model_reasoning_capability(&slug, model),
        );
        // The session's current effort seeds the suggestion only when a session
        // is attached; a pre-commit question has no session preference yet.
        let current_effort = session_id
            .and_then(|sid| self.session_metadata.get(&sid))
            .and_then(|m| m.reasoning_effort.clone());
        let suggested_effort = suggest_effort(
            current_effort.as_deref(),
            &capability.available_effort_levels,
        );
        debug!(
            account = %account,
            levels = capability.available_effort_levels.len(),
            has_suggestion = suggested_effort.is_some(),
            "ModelReasoning"
        );
        ModelReasoningResult {
            capability,
            suggested_effort,
        }
    }

    /// The catalog's curated model ids for `account`'s provider, used to paint
    /// the model picker instantly while the live `/v1/models` fetch runs. `None`
    /// when the account or its provider is unknown to the catalog, or the
    /// catalog lists no models for that provider — in which case
    /// `handle_list_models` answers with an empty provisional list (a waiting
    /// state, not an error).
    pub(super) fn catalog_models_for_account(&self, account: &str) -> Option<Vec<String>> {
        let slug = self.account_provider_slug(account)?;
        let entry = lookup_provider(&slug)?;
        if entry.models.is_empty() {
            return None;
        }
        let mut models: Vec<String> = entry.models.into_iter().map(|m| m.model).collect();
        // The seed is served as-is, so it must already carry the order every
        // other `Models` reply has (see `sort_model_ids`).
        sort_model_ids(&mut models);
        Some(models)
    }
}
