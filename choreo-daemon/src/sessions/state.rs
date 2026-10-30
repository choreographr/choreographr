//! The live session state and its per-request / per-subscriber sub-structures.
//!
//! [`SessionState`] is the single in-memory value a session's control thread
//! owns and mutates in response to [`SessionCommand`]s; [`AssistantResponse`]
//! bundles the fields the agent loop records onto a turn;
//! [`ActiveSessionEntry`] is the daemon's handle to a running session thread;
//! [`ActiveRequest`] tracks one in-flight request's cancellation handle.

use super::{
    Arc, AssistantToolCallRecord, BTreeMap, ClientId, DaemonCommand, DaemonMessageType,
    DisplayedImageRecord, HashMap, HashSet, ImageReference, InferenceProvider, LoadedSkill,
    ProjectToolSet, ReasoningArtifact, ReasoningProducer, RequestContext, ResolvedAccount,
    SessionCommand, SessionConfig, SessionEvent, SessionSnapshot, SkillMeta, SubscriberSink,
    TimestampMs, TokenUsage, ToolOutput, ToolResultRecord, Turn, WarmPolicy, broadcast, debug, io,
    model_reasoning_capability, write_turn_retry,
};

pub(crate) struct ActiveRequest {
    /// Cancellation channel for this request. The sender is held here and
    /// dropped only when the request is torn down (`RequestFinished`), so it
    /// outlives every worker wait that `select!`s on it: a firing cancel arm
    /// in `recv_sse_event`/the concurrent collector always means a real
    /// cancel message, never a disconnect. `sleep_or_cancel` (retry backoff)
    /// is the one deliberate exception — it treats a (theoretically
    /// unreachable) disconnect as "proceed without cancellation" rather than
    /// aborting a retry loop.
    pub(crate) cancel_tx: crossbeam_channel::Sender<()>,
    /// The `turn_id` this request is streaming into *right now*, so that
    /// late-joining subscribers can route streaming chunks to the correct turn.
    ///
    /// Seeded with the run's first turn when the request is accepted and kept
    /// current by [`handle_broadcast`](super::handle_broadcast), which observes
    /// the worker's per-iteration `Started` as the tool loop moves on to a new
    /// turn — a late joiner is handed this value in the synthetic `Started`
    /// `handle_attach` sends, and a stale value would route its chunks to a turn
    /// the run has already left.
    pub(crate) turn_id: u32,
}

/// A live session's control handles, held by the daemon's session manager.
pub struct ActiveSessionEntry {
    /// Channel to send [`SessionCommand`]s to the session's control thread.
    pub cmd_tx: crossbeam_channel::Sender<SessionCommand>,
    /// The session control thread's join handle, used to bound shutdown.
    pub handle: std::thread::JoinHandle<()>,
}

/// A live session's full in-memory state, owned by its control thread.
///
/// The persistent fields live in `config` (a `SessionConfig`); the rest is
/// runtime-only — turn history, the subscriber set, in-flight requests, the
/// lazily built provider client, and this session's private socket registry.
/// The whole value lives on exactly one thread and is mutated only in response
/// to a [`SessionCommand`].
pub struct SessionState {
    /// Persistent configuration fields (see `SessionConfig`).
    pub config: SessionConfig,
    /// Monotonic counter for turn ids.
    pub next_turn_id: u32,
    /// The next `stream_id` this session will assign to an accepted run. It is a
    /// per-session, monotonic counter owned by the session thread (never chosen
    /// by a client), so two clients' runs on the same session can never collide
    /// on a stream id. Starts at 1 so a real stream id is never the
    /// `CANCEL_ALL` sentinel (`0`).
    pub next_stream_id: u64,
    /// Stack of undo batches, newest last: each entry is the set of turns one
    /// `/undo` newly marked undone. `/redo` pops the most recent batch and
    /// restores exactly it, so undo and redo are LIFO-symmetric rather than a
    /// single redo collapsing every pending undo at once. A new user turn clears
    /// the stack (nothing is redoable once the conversation has moved on).
    pub(in crate::sessions) redo_stack: Vec<Vec<u32>>,
    /// The session's conversation turns, keyed by turn id.
    pub turns: BTreeMap<u32, Turn>,
    pub(in crate::sessions) subscribers: HashMap<ClientId, SubscriberSink>,
    /// The session's in-flight request(s), keyed by `stream_id`. A session runs
    /// at most one request at a time — the run handlers reject a second while
    /// this is non-empty ("session already has an active request") — so the map
    /// holds at most one entry; the `stream_id` key is what a cancel/lookup
    /// names the run by.
    pub(crate) active_requests: BTreeMap<u64, ActiveRequest>,
    /// The lazily built inference provider client for this session's account, or
    /// `None` before the first request resolves one (or after a
    /// `DropProvider`). Every socket it dials registers in this session's
    /// registry.
    pub provider: Option<InferenceProvider>,
    /// The account's **provider slug** (catalog key, e.g. "opencode-go"),
    /// recorded as soon as the account config resolves — at spawn time (the
    /// daemon command loop knows it from `AccountManager` with no credential
    /// involved), in [`SessionState::resolve_provider`], and on
    /// `SessionCommand::SetAccount` even before the keystore unlocks.
    ///
    /// Rationale: a model's static catalog facts (context window, reasoning
    /// capability) are pure catalog lookups keyed by the slug; they must NOT
    /// wait for the credential-bound `InferenceProvider` client to exist. The
    /// client is only built after unlock/first request, so resolving facts
    /// through it made the context window and effort cycling blink in and out
    /// of availability around keystore transitions.
    pub(in crate::sessions) provider_slug: Option<String>,
    /// This session's cache-warming policy — resolved by the daemon per account
    /// (the global `[cache_warming]` merged with the account's `meter`/
    /// `cache_warming`/`prompt_cache`) and refreshed whenever the session
    /// (re-)resolves its account (lazy first resolve, account switch, accounts
    /// reload). The agent loop reads it to decide whether to spawn a warmer; see
    /// [`crate::cache_warm`].
    pub warm_policy: WarmPolicy,
    /// This session's provider-socket registry. The provider client built for
    /// this session registers every dialed socket here, so closing the
    /// registry (cancel / suspend / keystore-lock) force-closes THIS
    /// session's connections and nothing else's — cancellation granularity
    /// is exactly the session.
    ///
    /// Design note: the registry deliberately travels together with
    /// `provider` (and, in future, the connection pool) as one per-session
    /// triple — agent + pool + registry. Future async tool calls will reuse
    /// this same triple so their sockets land in the same cancellable scope.
    pub registry: choreo_ai_protocols::SocketRegistry,
    /// Accumulated skill bodies from `load_skill` tool calls, injected into the
    /// system prompt on every turn.
    pub loaded_skill_bodies: Vec<LoadedSkill>,
    /// Cached context-bundle fingerprint and assembled text, so unchanged
    /// context files are not re-read from disk each turn.
    pub context_cache: Option<(u64, Arc<String>)>,
    /// The skill metadata discovered in the working directory, or `None` before
    /// discovery runs.
    pub discovered_skills: Option<Vec<SkillMeta>>,
    /// This session's private MCP tool set (project servers plus any
    /// `shared = false` per-session servers). Merged on top of the shared
    /// registry's definitions and consulted first on the execution path.
    pub project_tools: Arc<ProjectToolSet>,
    /// The daemon-tier `mcp/<slug>` groups this session's project shadows
    /// (removed from the shared registry's contribution to this session's tool
    /// list — a project server replaces the daemon-tier one by group).
    pub project_shadowed_groups: HashSet<String>,
    /// Turn ids this session has already written to the database in the current
    /// request: the agent loop's per-iteration checkpoints plus the finalize of
    /// the terminal turn (see `persist_turn_checkpoint` /
    /// `finalize_and_broadcast_turn`).
    ///
    /// Runtime-only. A request worker populates it as it persists turns and
    /// carries it back in the `RequestFinished` snapshot, so the merge can skip
    /// re-writing a row that already matches disk. Empty until a checkpoint runs
    /// (and always empty on the main session thread, which never checkpoints),
    /// so a turn that never reached disk is still written by the merge.
    pub persisted_turns: HashSet<u32>,
}

/// The assistant response recorded onto a turn by the agent loop: display
/// text + reasoning, tool calls, token usage, and the opaque reasoning
/// round-trip artifact with its producing model (see `ReasoningArtifact`).
///
/// Bundled into one value so the artifact + producer travel as a unit and
/// call sites stay readable instead of threading eight positional arguments
/// through [`SessionState::set_assistant_response`].
#[derive(Debug, Clone, Default)]
pub struct AssistantResponse {
    /// The assistant's display text, or `None` for a pure tool-use turn.
    pub text: Option<String>,
    /// The assistant's reasoning/thinking text, if the provider exposed it.
    pub reasoning: Option<String>,
    /// The tool calls the model requested, if any.
    pub tool_calls: Vec<AssistantToolCallRecord>,
    /// Token usage for this response, when the provider reported it.
    pub token_usage: Option<TokenUsage>,
    /// The opaque reasoning round-trip payload to replay to the provider on the
    /// next chained request, when one was captured.
    pub reasoning_artifact: Option<ReasoningArtifact>,
    /// The provider+model that produced `reasoning_artifact`, used for the
    /// same-model provenance check before replaying it.
    pub reasoning_producer: Option<ReasoningProducer>,
}

impl SessionState {
    /// The effective provider slug for static catalog-fact lookups (context
    /// window, reasoning capability). Prefers the live provider client's slug;
    /// falls back to the `provider_slug` field recorded with the account config,
    /// which is available BEFORE the keystore unlocks (no credential needed).
    /// `None` only while no account is bound to the session.
    ///
    /// Named distinctly from the `provider_slug` field so a call site reads
    /// clearly: the field is the recorded fact, this resolves the one to use
    /// right now.
    pub(in crate::sessions) fn effective_provider_slug(&self) -> Option<&str> {
        self.provider
            .as_ref()
            .map_or(self.provider_slug.as_deref(), |p| Some(p.provider_slug()))
    }

    /// Resolve the context window for `model`, keyed by provider slug rather
    /// than the credential-bound client, so the fact is available on a locked
    /// daemon / pre-first-request session. (Other static facts — e.g. the
    /// reasoning capability — resolve from [`Self::effective_provider_slug`].)
    ///
    /// With a live client this defers entirely to
    /// [`InferenceProvider::resolve_context_window`] (client-config override
    /// first, then the catalog through the client's slug). With no client — the
    /// locked/pre-first-request case — it is a pure catalog read keyed by the
    /// recorded slug, which is the whole reason the slug is stored.
    pub(in crate::sessions) fn resolve_context_window_for_model(&self, model: &str) -> Option<u32> {
        match &self.provider {
            Some(provider) => provider.resolve_context_window(model),
            None => self
                .provider_slug
                .as_deref()
                .and_then(|slug| choreo_ai_protocols::lookup_context_window(slug, model)),
        }
    }

    /// Re-resolve context window from the catalog when the stored value
    /// is `None` (e.g. sessions created before a model was added to the
    /// catalog, or after the provider was lazily resolved on unlock).
    pub(in crate::sessions) fn resolve_context_window_if_missing(&mut self, ctx: &RequestContext) {
        if self.config.context_window.is_some() {
            return;
        }
        let Some(model) = &self.config.selected_model else {
            return;
        };
        if let Some(cw) = self.resolve_context_window_for_model(model) {
            debug!(
                "session {}: re-resolved context_window={} for model={}",
                ctx.session_id, cw, model
            );
            self.config.context_window = Some(cw);
            broadcast(
                &mut self.subscribers,
                ctx,
                &DaemonMessageType::Session {
                    session_id: Some(ctx.session_id),
                    event: SessionEvent::ContextWindowResolved { context_window: cw },
                },
            );
        }
    }

    pub(in crate::sessions) fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            config: self.config.clone(),
            turns: self.turns.clone(),
            loaded_skill_bodies: self.loaded_skill_bodies.clone(),
            context_cache: self.context_cache.clone(),
            discovered_skills: self.discovered_skills.clone(),
            project_tools: Arc::clone(&self.project_tools),
            project_shadowed_groups: self.project_shadowed_groups.clone(),
            provider_slug: self.provider_slug.clone(),
            warm_policy: self.warm_policy,
            persisted_turns: self.persisted_turns.clone(),
        }
    }

    pub(in crate::sessions) fn from_snapshot(
        snapshot: SessionSnapshot,
        subscribers: HashMap<ClientId, SubscriberSink>,
    ) -> Self {
        // usize→u32 turn count: a session with 4 billion turns is impossible
        // in practice (each turn is a full provider round-trip).
        #[expect(clippy::cast_possible_truncation)]
        let turn_count = snapshot.turns.len() as u32;
        Self {
            config: snapshot.config,
            next_turn_id: turn_count,
            // A restored worker snapshot never accepts runs itself (the main
            // session thread assigns stream ids), so the counter only needs a
            // sentinel-safe starting value here.
            next_stream_id: 1,
            redo_stack: Vec::new(),
            turns: snapshot.turns,
            subscribers,
            active_requests: BTreeMap::new(),
            provider: None,
            provider_slug: snapshot.provider_slug,
            warm_policy: snapshot.warm_policy,
            // Restored snapshots never carry a live provider client; the next
            // request rebuilds one lazily against this fresh registry.
            registry: choreo_ai_protocols::SocketRegistry::default(),
            loaded_skill_bodies: snapshot.loaded_skill_bodies,
            context_cache: snapshot.context_cache,
            discovered_skills: snapshot.discovered_skills,
            project_tools: snapshot.project_tools,
            project_shadowed_groups: snapshot.project_shadowed_groups,
            persisted_turns: snapshot.persisted_turns,
        }
    }

    /// Build a [`SessionEvent::SessionState`] snapshot of the current session
    /// for broadcasting to connected clients.  Centralises the field mapping
    /// so that every broadcast site stays consistent when new fields are added.
    pub(crate) fn session_state_message(&self, session_id: u64) -> DaemonMessageType {
        let reasoning_capability = self.config.selected_model.as_ref().and_then(|model| {
            // Slug-keyed lookup (not the provider instance): the capability is
            // a static catalog fact and must be reported even while the
            // keystore is locked, so Alt+R works on an attached session
            // before any client has been built.
            let slug = self.effective_provider_slug()?;
            Some(model_reasoning_capability(slug, model))
        });
        DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionState {
                title: self.config.title.clone(),
                selected_model: self.config.selected_model.clone(),
                parent_session_id: self.config.parent_session_id,
                working_dir: self
                    .config
                    .working_dir
                    .as_ref()
                    .map(|p| p.display().to_string()),
                turns: self
                    .turns
                    .iter()
                    .map(|(&turn_id, turn)| (turn_id, turn_for_client(turn)))
                    .collect(),
                active_tool_groups: self.config.active_tool_groups.iter().cloned().collect(),
                token_usage: Some(self.config.accumulated_usage),
                context_window: self.config.context_window,
                last_prompt_tokens: self.config.last_prompt_tokens,
                status: self.config.status.clone(),
                reasoning_effort: self.config.reasoning_effort.clone(),
                reasoning_capability,
            },
        }
    }

    /// Start a new turn, returning its `turn_id`.
    /// If the turn has user text, the redo stack is cleared.
    pub fn start_turn(&mut self, user_text: Option<String>) -> (u32, Turn) {
        // New user input after an undo clears the redo stack.
        if user_text.is_some() {
            self.redo_stack.clear();
        }
        let turn_id = self.next_turn_id;
        self.next_turn_id += 1;
        let turn = Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        self.turns.insert(turn_id, turn.clone());
        (turn_id, turn)
    }

    /// Set the assistant response on a turn (text or tool-use).
    ///
    /// The response's `reasoning_artifact`/`reasoning_producer` record the
    /// opaque reasoning round-trip payload and the model that produced it
    /// (phase 4b/4c). The producer is set whenever the model completes a
    /// response — even when the artifact is None (no reusable payload) — so
    /// the builder's same-model provenance check is well-defined for every
    /// turn. See [`AssistantResponse`].
    pub fn set_assistant_response(&mut self, turn_id: u32, response: AssistantResponse) {
        if let Some(turn) = self.turns.get_mut(&turn_id) {
            turn.assistant_text = response.text;
            turn.assistant_reasoning = response.reasoning;
            turn.tool_calls = response.tool_calls;
            turn.token_usage = response.token_usage;
            turn.reasoning_artifact = response.reasoning_artifact;
            turn.reasoning_producer = response.reasoning_producer;
        }
    }

    /// Seed placeholder tool results for every tool call, in call order, so
    /// the transcript always renders tool results in the order the model
    /// issued them — even while the tools are still running. Each placeholder
    /// is filled in place by [`Self::update_tool_result`] the moment its tool
    /// streams or finishes, so the rendered order never changes.
    ///
    /// `invocation_descriptions` runs parallel to `tool_calls` (same length,
    /// same order — both are derived from the same model response): seeding
    /// the description onto each placeholder means clients render the tool's
    /// context (e.g. "Running command: `…`.") from the moment the turn is
    /// broadcast, matching the final record's header exactly.
    pub fn seed_tool_results(
        &mut self,
        turn_id: u32,
        tool_calls: &[AssistantToolCallRecord],
        invocation_descriptions: &[String],
    ) {
        if let Some(turn) = self.turns.get_mut(&turn_id) {
            turn.tool_results = tool_calls
                .iter()
                .zip(invocation_descriptions.iter())
                .map(|(tc, desc)| ToolResultRecord {
                    call_id: tc.call_id.clone(),
                    name: tc.name.clone(),
                    content: String::new(),
                    is_error: false,
                    invocation_description: desc.clone(),
                    image: None,
                })
                .collect();
        }
    }

    /// Set (or replace) a single tool result in place, matched by `call_id`,
    /// so the result keeps its position in the model's call order regardless
    /// of when the tool actually finished. Requires the turn to have been
    /// seeded via [`Self::seed_tool_results`]; otherwise it is a no-op.
    ///
    /// The five per-record fields (content, `is_error`, `invocation_description`,
    /// image) are collapsed into the `output` value so the signature stays
    /// under clippy's `too_many_arguments` threshold; `name` stays explicit
    /// because `ToolOutput` does not carry it (it comes from the tool call).
    pub fn update_tool_result(
        &mut self,
        turn_id: u32,
        call_id: &str,
        name: String,
        output: &ToolOutput,
    ) {
        if let Some(turn) = self.turns.get_mut(&turn_id)
            && let Some(record) = turn.tool_results.iter_mut().find(|r| r.call_id == call_id)
        {
            record.name = name;
            record.content.clone_from(&output.content);
            record.is_error = output.is_error;
            record
                .invocation_description
                .clone_from(&output.invocation_description);
            record.image.clone_from(&output.image_ref);
        }
    }

    /// Mark the tool results whose outcome was never recorded because the
    /// request was cancelled.
    ///
    /// Placeholders are seeded for every call before any tool executes, so a
    /// request cancelled mid-execution (e.g. Escape during the serial phase)
    /// leaves empty slots for calls that never ran *and* for calls that were
    /// dispatched but still running when the request stopped. Fill them with
    /// an explicit marker so the transcript shows what happened and the next
    /// provider request does not carry empty tool messages for calls whose
    /// outcome is unknown. `executed` holds the `call_ids` whose results were
    /// actually recorded; every other placeholder is marked.
    pub fn mark_unexecuted_tool_results(&mut self, turn_id: u32, executed: &HashSet<String>) {
        if let Some(turn) = self.turns.get_mut(&turn_id) {
            for record in &mut turn.tool_results {
                if !executed.contains(&record.call_id) {
                    record.content = "[cancelled — result not recorded]".to_string();
                    record.is_error = true;
                }
            }
        }
    }

    /// Add a displayed image to a turn.
    pub fn add_displayed_image(&mut self, turn_id: u32, record: DisplayedImageRecord) {
        if let Some(turn) = self.turns.get_mut(&turn_id) {
            turn.displayed_images.push(record);
        }
    }

    /// Set an error on a turn.
    pub fn set_turn_error(&mut self, turn_id: u32, error: String) {
        if let Some(turn) = self.turns.get_mut(&turn_id) {
            turn.error = Some(error);
        }
    }

    /// Finalize a turn and persist it to the database.
    /// Returns an error if persistence fails after all retries.
    ///
    /// This is the one write of a whole turn — its zstd-compressed blob plus
    /// every attachment slot, in a single transaction — so the on-disk turn can
    /// never be observed half-written. The agent loop calls it once per turn:
    /// through `persist_turn_checkpoint` at its iteration boundary (a
    /// crash-safety checkpoint) for a tool-use turn, or through
    /// `finalize_and_broadcast_turn` for the terminal turn. A turn written this
    /// way is then also re-written by the session thread's `RequestFinished`
    /// merge (`write_turn_retry`); both writes are idempotent, so re-writing an
    /// unchanged turn costs one transaction and changes nothing — the callers
    /// record the id in [`SessionState::persisted_turns`] so the merge can skip
    /// that redundant rewrite when the turn is unchanged.
    ///
    /// Takes `&self`: persisting reads the turn out of `turns`, it never
    /// mutates the session (the callers record the persisted id).
    ///
    /// # Errors
    ///
    /// Returns Err if the turn does not exist, the session write fails
    /// after retries, or encoding/persisting any displayed image fails.
    pub fn finalize_turn(
        &self,
        db: &redb::Database,
        session_id: u64,
        turn_id: u32,
    ) -> io::Result<()> {
        if let Some(turn) = self.turns.get(&turn_id) {
            write_turn_retry(db, session_id, turn_id, turn)
                .map_err(|e| io::Error::other(format!("failed to persist turn {turn_id}: {e}")))?;
        }
        Ok(())
    }

    /// Undo the most recent user-initiated turn: find the most recent
    /// non-undone turn with `user_text: Some(...)`, mark it and every higher-id
    /// turn that is still live as `undone = true`, and push that batch onto the
    /// redo stack.
    ///
    /// Only the turns this call actually flips (the live suffix) are recorded
    /// for redo: turns already hidden by an earlier undo stay hidden until their
    /// own batch is popped, so a single `/redo` re-applies exactly one `/undo`
    /// rather than collapsing every pending undo at once.
    pub fn undo_turns(&mut self) -> Option<Vec<u32>> {
        let target = self
            .turns
            .iter()
            .rev()
            .find(|(_, t)| !t.undone && t.user_text.is_some())
            .map(|(&id, _)| id)?;
        let mut newly_undone = Vec::new();
        for (&id, turn) in self.turns.range_mut(target..) {
            if !turn.undone {
                turn.undone = true;
                newly_undone.push(id);
            }
        }
        self.redo_stack.push(newly_undone.clone());
        // The context size is a function of the live turn set, so it must be
        // re-derived whenever history mutates — not just refreshed by the next
        // completed request. Undoing turns shrinks the context.
        self.config.last_prompt_tokens = self.live_last_prompt_tokens();
        Some(newly_undone)
    }

    /// Redo the most recent `/undo`: pop its batch and restore exactly those
    /// turns, leaving any turns hidden by earlier undos still hidden.
    pub fn redo_turns(&mut self) -> Option<BTreeMap<u32, Turn>> {
        let ids = self.redo_stack.pop()?;
        let mut restored = BTreeMap::new();
        for id in ids {
            if let Some(turn) = self.turns.get_mut(&id) {
                turn.undone = false;
                restored.insert(id, turn.clone());
            }
        }
        // Redoing turns grows the context back; re-derive from the restored
        // live set (mirrors `undo_turns`).
        self.config.last_prompt_tokens = self.live_last_prompt_tokens();
        Some(restored)
    }

    /// Create an empty session state.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            config: SessionConfig::default(),
            next_turn_id: 0,
            // Stream ids start at 1: 0 is the `CANCEL_ALL` sentinel, so a real
            // stream id must never be 0.
            next_stream_id: 1,
            redo_stack: Vec::new(),
            turns: BTreeMap::new(),
            subscribers: HashMap::new(),
            active_requests: BTreeMap::new(),
            provider: None,
            provider_slug: None,
            warm_policy: WarmPolicy::default(),
            // A fresh empty registry: any provider client built for this
            // state registers its sockets here, so cancelling this session
            // (or dropping it) never touches another session's connections.
            registry: choreo_ai_protocols::SocketRegistry::default(),
            loaded_skill_bodies: Vec::new(),
            context_cache: None,
            discovered_skills: None,
            project_tools: Arc::new(ProjectToolSet::empty()),
            project_shadowed_groups: HashSet::new(),
            persisted_turns: HashSet::new(),
        }
    }

    /// Lazily resolve (and cache) this session's inference provider against
    /// THIS session's socket registry.
    ///
    /// Sessions can be created while the keystore is locked, so no provider
    /// exists at creation time; instead the first request asks the daemon
    /// command loop for the account's config + resolved auth grant (the daemon
    /// is the sole owner of the credential map — the cleartext grant never
    /// leaves it) and builds the client HERE, on the session thread, so every
    /// socket it dials lands in `self.registry`.
    ///
    /// A proactive OAuth refresh that fails transiently does not fail the
    /// request when a cached access token is still available: the client is
    /// built with that token and the normal 401 → force-refresh path takes
    /// over if it has expired. Only a terminally dead grant, or a transient
    /// failure with no cached token to fall back on, surfaces an error.
    ///
    /// Error strings are the SAME user-facing messages the old daemon-side
    /// cache produced, so clients see identical guidance.
    pub(crate) fn resolve_provider(
        &mut self,
        ctx: &RequestContext,
    ) -> Result<InferenceProvider, String> {
        if let Some(p) = &self.provider {
            return Ok(p.clone());
        }
        let Some(name) = self.config.account_name.clone() else {
            return Err(
                "no account configured on this session — use /account <name> to set one"
                    .to_string(),
            );
        };
        // `None` from the daemon covers every failure the old provider cache
        // hid behind the same reply shape: unknown account, keystore locked,
        // or no credential stored. Keep the exact wording those paths used.
        // (Crossbeam per AGENTS.md: this reply may outlive the quick path —
        // e.g. a dropped channel when the session dies mid-request — and the
        // `Zeroizing` wrappers inside `ProviderAuth` wipe any unconsumed grant
        // from the queue on drop.)
        let Some(ResolvedAccount {
            config,
            auth,
            warm_policy,
        }) = Self::send_resolve_account(ctx, &name)
        else {
            // Unknown/locked account: reset to the conservative policy so a
            // removed account cannot keep warming on a stale meter.
            self.warm_policy = WarmPolicy::default();
            return Err(format!(
                "no credential stored for account '{name}' — add one via the AI Providers page or /add-key"
            ));
        };
        // The warm policy and provider slug are non-secret facts the daemon
        // resolved for this account; record them BEFORE the auth check so
        // warming and static catalog lookups (context window, reasoning
        // capability) stay correct even on this failed resolution.
        self.warm_policy = warm_policy;
        self.provider_slug = Some(config.provider.clone());
        // Proactive OAuth refresh: an OAuth account whose cached access token is
        // absent or within the expiry buffer is refreshed BEFORE the client is
        // built — the client needs a live bearer, and the constructor would
        // otherwise fail with "sign in first". A successful refresh persists the
        // token via the daemon, so re-resolve to pick it up.
        let (config, auth) = if matches!(auth, Some(crate::providers::ProviderAuth::OAuth { .. })) {
            match self.refresh_oauth_token(false, ctx) {
                Ok(crate::oauth_refresh::RefreshTokenOutcome::Refreshed) => {
                    match Self::send_resolve_account(ctx, &name) {
                        Some(ResolvedAccount {
                            config,
                            auth,
                            warm_policy,
                        }) => {
                            self.warm_policy = warm_policy;
                            self.provider_slug = Some(config.provider.clone());
                            (config, auth)
                        }
                        None => (config, auth),
                    }
                }
                Ok(_) => (config, auth),
                Err(failure) => match failure {
                    // A terminally dead grant names re-login (the daemon already
                    // broadcast the notice): no token can revive it.
                    crate::oauth_refresh::RefreshFailure::Terminal => {
                        return Err(format!(
                            "OAuth grant for account '{name}' was rejected; sign in again"
                        ));
                    }
                    crate::oauth_refresh::RefreshFailure::Transient { detail } => {
                        // A transient blip is not fatal while a cached access token
                        // is still in hand: the refresh only fires when the token is
                        // absent or within the expiry buffer, so a within-buffer
                        // token is usually still valid server-side. Proceed with it —
                        // the first request 401s and drives the force-refresh path if
                        // it is genuinely expired. With no cached token there is no
                        // bearer to try, so surface the refresh error instead.
                        if matches!(
                            auth,
                            Some(crate::providers::ProviderAuth::OAuth {
                                access_token: Some(_),
                                ..
                            })
                        ) {
                            tracing::warn!(
                                session = ctx.session_id,
                                account = %name,
                                detail = %detail,
                                "proactive OAuth refresh failed transiently; trying the cached access token"
                            );
                            (config, auth)
                        } else {
                            return Err(format!(
                                "OAuth refresh for account '{name}' failed: {detail}"
                            ));
                        }
                    }
                },
            }
        } else {
            (config, auth)
        };
        let Some(auth) = auth else {
            return Err(format!(
                "no credential stored for account '{name}' — add one via the AI Providers page or /add-key"
            ));
        };
        // The client constructor takes the resolved grant (API key or OAuth);
        // the refresh token rides inside `ProviderAuth` but is consumed to build
        // only the request material, so it never reaches the client.
        let provider = InferenceProvider::from_account_config(&config, auth, &self.registry)
            .map_err(|e| {
                tracing::warn!(
                    session = ctx.session_id,
                    account = %name,
                    error = %e,
                    "failed to build provider client for session"
                );
                // Surface the specific reason (no API key for this protocol,
                // an OAuth account with no access token yet, an unsupported
                // protocol) — it is more actionable than the old generic line,
                // and for an OAuth account it names the sign-in step.
                e
            })?;
        debug!(session = ctx.session_id, account = %name, "resolved session provider lazily");
        self.provider = Some(provider.clone());
        Ok(provider)
    }

    /// Ask the daemon to resolve `name`'s config + auth grant, blocking for the
    /// reply. `None` covers unknown account, locked keystore, and no credential.
    fn send_resolve_account(ctx: &RequestContext, name: &str) -> Option<ResolvedAccount> {
        let (reply, rx) = crossbeam_channel::unbounded();
        let _ = ctx.daemon_tx.send(DaemonCommand::ResolveAccountCmd {
            account: name.to_string(),
            reply,
        });
        rx.recv().ok().flatten()
    }

    /// Refresh this session's OAuth access token when it is absent or within the
    /// expiry buffer (or unconditionally when `force`), serialized against other
    /// refreshes of the same account by the daemon's single-flight registry.
    ///
    /// The blocking exchange runs on THIS thread; the daemon owns the cache and
    /// the blob write (see `crate::oauth_refresh`). On a successful refresh the
    /// cached client is dropped so the next resolve rebuilds it with the new
    /// token.
    ///
    /// # Errors
    ///
    /// Returns [`crate::oauth_refresh::RefreshFailure`] on a terminal (re-login
    /// required) or transient failure.
    pub(crate) fn refresh_oauth_token(
        &mut self,
        force: bool,
        ctx: &RequestContext,
    ) -> Result<crate::oauth_refresh::RefreshTokenOutcome, crate::oauth_refresh::RefreshFailure>
    {
        let Some(account) = self.config.account_name.clone() else {
            return Ok(crate::oauth_refresh::RefreshTokenOutcome::NotApplicable);
        };
        let source = crate::oauth_refresh::DaemonTokenSource::new(ctx.daemon_tx.clone());
        let outcome =
            crate::oauth_refresh::refresh_single_flight(&source, &account, force, unix_secs_now())?;
        if outcome == crate::oauth_refresh::RefreshTokenOutcome::Refreshed {
            // The cached client holds the old token; drop it so the next resolve
            // rebuilds with the refreshed one.
            self.provider = None;
        }
        Ok(outcome)
    }
}

/// The current time as Unix seconds, saturating rather than wrapping on a
/// clock set before the epoch.
fn unix_secs_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Client-bound copy of a turn with the opaque reasoning round-trip payload
/// and the image **bytes** stripped: only the daemon consumes
/// `reasoning_artifact`/`reasoning_producer` (it rebuilds the next provider
/// request from them), the vision image **bytes** in `ToolResultRecord.image`
/// (the request builder reads them from the authoritative daemon-side
/// `SessionState`/DB), and `DisplayedImageRecord.data` (the bytes live in the
/// DB; clients fetch them on demand via `ClientMessageType::GetImage`).
///
/// Image **metadata** stays on the client view for BOTH kinds: each
/// `DisplayedImageRecord` keeps its `metadata` (dimensions, mime, `byte_len`,
/// alt), and a `ToolResultRecord.image` keeps its `ImageReference` with `data`
/// emptied (path, mime, dimensions) — all the clients need to lay out a
/// placeholder and know there is an attachment to fetch, so a long session's
/// history no longer ships every image up front. Stripping the bytes keeps the
/// artifact and every image's bytes off each `DaemonMessage` payload
/// (bandwidth + privacy: thinking-block JSON, encrypted provider blobs, and
/// raw image bytes never ride the snapshots/broadcasts), while the
/// authoritative `Turn` in `SessionState` and the DB keeps the full payload for
/// the request builder and the image store.
pub(crate) fn turn_for_client(turn: &Turn) -> Turn {
    // Reconstruct the client turn FIELD BY FIELD instead of `turn.clone()`:
    // a deep clone would COPY every image payload (display + vision bytes, up
    // to megabytes) into the clone only for the lines below to throw it away.
    // This is the hot path — `session_state_message` runs it over every turn
    // of a session on open, and `broadcast_turn_appended` runs it after every
    // streamed tool result / image emit — so the redundant payload copy would
    // make an N-image turn O(N²) in server-side memory traffic (the wire is
    // already O(N) after stripping). Building the stripped records directly
    // never allocates the image buffers at all. (A new `Turn` field forces a
    // compile error here, which is deliberate: it must be triaged into the
    // client view rather than silently deep-cloned.)
    Turn {
        created_at: turn.created_at,
        undone: turn.undone,
        error: turn.error.clone(),
        user_text: turn.user_text.clone(),
        assistant_text: turn.assistant_text.clone(),
        assistant_reasoning: turn.assistant_reasoning.clone(),
        tool_calls: turn.tool_calls.clone(),
        token_usage: turn.token_usage,
        // Vision image BYTES are daemon/model-only: the request builder
        // consumes them from the authoritative daemon-side turn. But the client
        // still learns the image EXISTS (and its dimensions/mime) so it can
        // render a placeholder and fetch the bytes on demand — the reference
        // rides the client view with `data` emptied, exactly like a
        // `DisplayedImageRecord`'s bytes are stripped but its metadata kept.
        tool_results: turn
            .tool_results
            .iter()
            .map(|r| ToolResultRecord {
                call_id: r.call_id.clone(),
                name: r.name.clone(),
                content: r.content.clone(),
                is_error: r.is_error,
                invocation_description: r.invocation_description.clone(),
                image: r.image.as_ref().map(|img| ImageReference {
                    path: img.path.clone(),
                    mime_type: img.mime_type.clone(),
                    width: img.width,
                    height: img.height,
                    data: Vec::new(),
                }),
            })
            .collect(),
        // Displayed-image bytes are fetched on demand
        // (ClientMessageType::GetImage); the metadata (with `byte_len`) stays so
        // the client can size the placeholder and knows whether there is
        // anything to fetch.
        displayed_images: turn
            .displayed_images
            .iter()
            .map(|img| DisplayedImageRecord {
                metadata: img.metadata.clone(),
                data: Vec::new(),
                tool_call_id: img.tool_call_id.clone(),
            })
            .collect(),
        // Opaque reasoning round-trip payloads are daemon-only.
        reasoning_artifact: None,
        reasoning_producer: None,
    }
}
