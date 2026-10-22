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
//!   from the cache, from the local catalog's curated ids, or with the
//!   retryable "warming" error — it only *triggers* a background prefetch,
//!   whose landing broadcast (`ModelsUpdated`) replaces whatever an already-open
//!   picker painted.
//!
//! `ValidateModel` is the fail-open consumer of the same cache: a model is
//! rejected only when a cached list exists and does not contain it, because a
//! list we could not fetch is no evidence that the model is invalid.

use super::{DaemonCommand, DaemonMessageType, DaemonState};
use choreo_ai_protocols::lookup_provider;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// TTL for cached provider model lists. Shared by the freshness checks in
/// [`DaemonState::handle_list_models`] and the background-prefetch guard
/// ([`DaemonState::should_prefetch_models`]) so both paths agree on what
/// "fresh" means.
pub(super) const MODEL_CACHE_TTL: Duration = Duration::from_secs(300);

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
        match self.model_cache.get(account) {
            Some((_, cached_at)) => Instant::now().duration_since(*cached_at) >= MODEL_CACHE_TTL,
            None => true,
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
    /// loop is its single writer).  Failures are logged only — the next
    /// session join re-prefetches, and the on-demand path in
    /// [`Self::handle_list_models`] remains the fallback while nothing is
    /// cached.
    pub(super) fn handle_model_prefetch_result(
        &mut self,
        account: String,
        result: Result<Vec<String>, String>,
    ) {
        self.model_prefetch_in_flight.remove(&account);
        match result {
            Ok(models) => {
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
                // Announce the freshly fetched list so an already-open model
                // picker replaces its list (a catalog seed, or an empty
                // "warming" state) in place instead of waiting for the user to
                // reopen it. The broadcast is keyed by account name; a frontend
                // applies it only to the picker showing that account.
                let updated = DaemonMessageType::ModelsUpdated {
                    account: account.clone(),
                    models: models.clone(),
                };
                self.model_cache.insert(account, (models, Instant::now()));
                self.broadcast(&updated);
            }
            Err(e) => {
                warn!(
                    account = %account,
                    error = %e,
                    "background model prefetch failed; will retry on the next join"
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
        model: &str,
        reply: &mpsc::Sender<Result<(), String>>,
    ) {
        debug!("ValidateModel: session_id={}, model={}", session_id, model);

        let Some(account_name) = self
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
                if cached_models.iter().any(|m| m == model) {
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

    /// The `ListModels` handler: resolve the session's account, then answer
    /// with the best model list available WITHOUT ever performing a fetch on
    /// the command loop.
    ///
    /// A fresh cache answers immediately; otherwise the fetch is handed to a
    /// detached background thread (dedup-guarded) and the reply is the stale
    /// cached list, the local catalog's curated ids, or — when neither exists —
    /// the retryable "warming" error from `choreo_proto`. The selected model
    /// rides along so a picker can mark the active row.
    pub(super) fn handle_list_models(
        &mut self,
        session_id: Option<u64>,
    ) -> Result<(Vec<String>, Option<String>), String> {
        let account_name = session_id
            .and_then(|sid| self.session_metadata.get(&sid))
            .and_then(|m| m.account_name.clone())
            .unwrap_or_default();

        debug!(
            "ListModels: session_id={:?}, account_name='{}', accounts={:?}",
            session_id,
            account_name,
            self.accounts
                .all_configs()
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>()
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

        // A fresh cache answers immediately. Otherwise the fetch NEVER runs here
        // synchronously: a blocking HTTP round-trip (up to the full request
        // timeout, retried) would stall the whole daemon command loop — the
        // exact stall the background-prefetch design removed from unlock. The
        // request instead TRIGGERS a background prefetch (dedup-guarded via
        // `maybe_spawn_model_prefetch` — no-op when one is already running, so
        // an open picker while a join-time prefetch is in flight does not
        // double-fetch) and serves the best list it can WITHOUT blocking:
        //   - a fresh cache answers immediately;
        //   - else a stale-but-present list is served (it beats nothing);
        //   - else the local catalog's curated ids for the account's provider are
        //     served as an instant first paint, so the picker opens without
        //     waiting on a slow endpoint (api.openai.com's /v1/models can take
        //     ~10 s per attempt and trip a 504 retry);
        //   - only when none of those exist does it return the retryable
        //     "warming" error.
        // When the background fetch lands it broadcasts `ModelsUpdated`, which
        // replaces whatever was painted in an already-open picker.
        let now = Instant::now();
        let models = match self.model_cache.get(&account_name) {
            Some((cached_models, cached_at))
                if now.duration_since(*cached_at) < MODEL_CACHE_TTL =>
            {
                cached_models.clone()
            }
            _ => {
                // Clone the stale list (if any) BEFORE the mutable spawn call,
                // so no borrow of `self` is live across it. An empty cached list
                // is treated as absent so a catalog seed can stand in for it.
                let stale = self
                    .model_cache
                    .get(&account_name)
                    .map(|(models, _)| models.clone())
                    .filter(|models| !models.is_empty());
                // Paint from the local catalog only when there is no cached list
                // at all: a present (even stale) list is more account-specific
                // than the catalog's provider-wide curated set.
                let seed = if stale.is_none() {
                    catalog_models_for_account(self, &account_name)
                } else {
                    None
                };
                self.maybe_spawn_model_prefetch(&account_name);
                stale
                    .or(seed)
                    .ok_or_else(|| choreo_proto::models_warming_error(&account_name))?
            }
        };

        let selected_model = session_id
            .and_then(|sid| self.session_metadata.get(&sid))
            .and_then(|m| m.selected_model.clone());
        Ok((models, selected_model))
    }
}

/// The catalog's curated model ids for an account's provider, used to paint
/// the model picker instantly while the live `/v1/models` fetch runs. `None`
/// when the account or its provider is unknown to the catalog, or the catalog
/// lists no models for that provider — in which case `handle_list_models`
/// falls back to the retryable "warming" error rather than an empty list.
pub(super) fn catalog_models_for_account(
    state: &DaemonState,
    account: &str,
) -> Option<Vec<String>> {
    let slug = state.account_provider_slug(account)?;
    let entry = lookup_provider(&slug)?;
    if entry.models.is_empty() {
        return None;
    }
    Some(entry.models.into_iter().map(|m| m.model).collect())
}
