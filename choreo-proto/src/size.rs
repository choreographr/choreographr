//! Lag-eviction byte gauge: cheap, conservative estimates of the serialized
//! wire size of [`Turn`] and [`DaemonMessage`].
//!
//! These estimates are the threshold the daemon's lag accounting
//! (`choreo-daemon::broadcast::SubscriberSink`) compares against the
//! per-client cap / global budget, so they must never UNDER-estimate the
//! encoded payload (a genuinely lagging client could then escape eviction).
//! They deliberately over-estimate: over-counting only makes the lag limit
//! slightly conservative.
//!
//! MAINTENANCE: the per-arm `named_field_overhead(n)` field counts, the
//! per-record allowances in [`Turn::approx_size`], and the fixed
//! [`DaemonMessage::approx_wire_size`] envelope are hand-tuned against the
//! MessagePack named-mode encoder. When a serialized struct/variant gains a
//! field, or a record type's shape changes, update the count/allowance HERE
//! in the same change — the pin test
//! `types::tests::approx_wire_size_never_underestimates_encoded_payload`
//! (in `types.rs`) encodes every variant with dense payloads and fails on
//! any under-estimate, so a drift surfaces as a test failure.

use crate::{
    DaemonMessage, OutputStream, ReasoningArtifact, ReasoningCapability, SessionStatus, Turn,
};

impl Turn {
    /// Cheap, conservative estimate of this turn's serialized byte size.
    ///
    /// Used by [`DaemonMessage::approx_wire_size`] (daemon-side lag
    /// accounting). Sums the variable-size fields — text, tool-call
    /// arguments, tool-result content, and image binary data — plus a fixed
    /// per-record overhead, and deliberately over-estimates: it is a
    /// threshold gauge, so over-counting only makes the lag limit slightly
    /// conservative, while under-counting could let a genuinely lagging
    /// client escape eviction. Fixed-size scalars (timestamps, token counts,
    /// flags) are ignored.
    pub fn approx_size(&self) -> usize {
        // Fixed per-turn overhead from `named_field_overhead(13)` (the 13
        // named-mode field-name keys + tags; an all-None turn serializes to
        // ~200 bytes). The arms below add only payload bytes on top, so the
        // estimate always over-counts: it is a threshold gauge, where
        // over-counting just makes the lag limit slightly conservative.
        let mut size = named_field_overhead(13);
        if let Some(s) = &self.error {
            size += s.len();
        }
        if let Some(s) = &self.user_text {
            size += s.len();
        }
        if let Some(s) = &self.assistant_text {
            size += s.len();
        }
        if let Some(s) = &self.assistant_reasoning {
            size += s.len();
        }
        for call in &self.tool_calls {
            // Real MessagePack named-mode cost per `AssistantToolCallRecord` is
            // ~57 B of overhead (map header + variant tag + the three field-
            // name keys), so a 16 B allowance under-counted tool-heavy turns:
            // the lag gauge must never UNDER-estimate the serialized payload
            // (a genuinely lagging client could then escape eviction). 64 B
            // leaves comfortable slack for longer keys/payload headers.
            size += 64 + call.call_id.len() + call.name.len() + call.arguments_json.len();
        }
        for result in &self.tool_results {
            // Real per-`ToolResultRecord` overhead is ~79 B (five field keys,
            // the longest being `invocation_description` at 21 chars); the
            // old 32 B allowance under-counted by ~47 B per result. 96 B
            // over-covers it, keeping the gauge conservative.
            size += 96
                + result.call_id.len()
                + result.name.len()
                + result.content.len()
                + result.invocation_description.len();
        }
        for image in &self.displayed_images {
            // Real per-`DisplayedImageRecord` overhead is ~127 B (the nested
            // `ImageMetadata` struct adds its own tag + five field keys); the
            // old 48 B allowance under-counted by ~79 B per image. 160 B
            // over-covers it.
            size += 160
                + image.data.len()
                + image.metadata.mime_type.len()
                + image.metadata.alt.as_ref().map_or(0, String::len);
        }
        if let Some(artifact) = &self.reasoning_artifact {
            // Opaque round-trip payload: count the raw bytes, tagged by the
            // variant that owns them. Real overhead is ~40 B for the bare-byte
            // variants but ~88 B for `ChatReasoning` (an extra nested map +
            // the `field` enum tag), so 96 B covers every variant with slack.
            let payload = match artifact {
                ReasoningArtifact::ChatReasoning { bytes, .. } => bytes.len(),
                ReasoningArtifact::AnthropicThinking(bytes)
                | ReasoningArtifact::GoogleSignatures(bytes)
                | ReasoningArtifact::ResponsesItems(bytes) => bytes.len(),
            };
            size += 96 + payload;
        }
        if let Some(producer) = &self.reasoning_producer {
            // Real per-`ReasoningProducer` overhead is ~60 B (its own variant
            // tag + two field keys); the old 16 B allowance under-counted.
            size += 64 + producer.provider_slug.len() + producer.model.len();
        }
        size
    }
}

/// Byte length of an `Option<String>`'s payload (0 when None). Shared by
/// [`DaemonMessage::approx_wire_size`] so every optional string field is
/// counted the same way. Only the payload is counted here — the MessagePack
/// field-name key and length prefix are covered by the per-field allowance of
/// [`named_field_overhead`], which the arm's fixed base provides.
fn option_str_len(s: &Option<String>) -> usize {
    s.as_ref().map_or(0, String::len)
}

/// Estimated MessagePack named-mode overhead for a struct variant with `n`
/// fields: the variant tag, map headers, and one field-name key per field.
/// The allowance is ~24 bytes per field (the longest field-name keys in use
/// are ~21 chars plus a length prefix; the value side is a nil marker, a
/// scalar, or a string header — all covered here) plus a fixed tag/header
/// allowance, so the base alone covers every field's key + value tag whether
/// the value is absent or present. Arms then add only the variable PAYLOAD
/// bytes on top. Over-estimating a little is the conservative direction for
/// a lag-eviction gauge: under-counting could let a lagging client escape
/// eviction.
fn named_field_overhead(fields: usize) -> usize {
    40 + fields * 24
}

/// Byte length of a `SessionStatus` payload (a small enum; only the
/// `ToolCall(String)` and `Retrying` variants carry data).
fn session_status_size(status: &SessionStatus) -> usize {
    match status {
        SessionStatus::ToolCall(name) => 8 + name.len(),
        SessionStatus::Retrying { .. } => 16,
        SessionStatus::Sleeping | SessionStatus::Inactive | SessionStatus::Inference => 4,
    }
}

/// Byte length of a `ReasoningCapability` payload: the named-mode variant
/// tag, map header, `available_effort_levels` field key, the array header,
/// and one string header per effort level, plus the level slugs' payloads.
/// Fixed at ~47 B for a 4-level set, so the 64 B fixed part over-covers it
/// and the per-element +1 accounts for longer level lists.
fn reasoning_capability_size(cap: &ReasoningCapability) -> usize {
    64 + cap.available_effort_levels.len()
        + cap
            .available_effort_levels
            .iter()
            .map(String::len)
            .sum::<usize>()
}

impl DaemonMessage {
    /// Cheap, conservative estimate of this message's serialized byte size.
    ///
    /// Used by the daemon's lag accounting (broadcast::SubscriberSink) to
    /// gauge how many bytes a slow client has fallen behind the streaming
    /// frontier. It
    /// deliberately over-estimates rather than serializes: the accounting is
    /// a threshold gauge, so a slightly-too-big estimate can only trigger
    /// eviction a little early — never let a genuinely lagging client slip
    /// past the limit. O(1) in message count — it sums the variable-size
    /// fields (strings, byte buffers, vecs, turn maps) plus a fixed
    /// per-variant envelope overhead, and ignores fixed-size scalars.
    /// The over-estimate is pinned by the
    /// `approx_wire_size_never_underestimates_encoded_payload` test in `types.rs`,
    /// which encodes every `DaemonMessage` variant with realistic payloads and
    /// asserts the estimate covers the actual bytes.
    pub fn approx_wire_size(&self) -> usize {
        // Fixed envelope overhead for FIELDLESS variants (Pong, Evicted, …):
        // just the variant tag, so this generous fixed value is a comfortable
        // over-estimate. Struct variants get their per-field named-mode key
        // overhead from `named_field_overhead` instead.
        const OVERHEAD: usize = 96;
        match self {
            Self::SessionCreated {
                title,
                working_dir,
                account_name,
                selected_model,
                reasoning_effort,
                ..
            } => {
                named_field_overhead(6)
                    + option_str_len(title)
                    + option_str_len(working_dir)
                    + option_str_len(account_name)
                    + option_str_len(selected_model)
                    + option_str_len(reasoning_effort)
            }
            Self::Sessions { sessions } => {
                named_field_overhead(1)
                    + sessions
                        .iter()
                        .map(|s| {
                            // 15-field SessionSummary: named-mode keys + tags
                            // + status string (the variable-size status
                            // payload is not otherwise counted).
                            named_field_overhead(15)
                                + option_str_len(&s.title)
                                + option_str_len(&s.selected_model)
                                + option_str_len(&s.reasoning_effort)
                                + option_str_len(&s.working_dir)
                                + option_str_len(&s.account_name)
                                + s.active_tool_groups.iter().map(String::len).sum::<usize>()
                                + session_status_size(&s.status)
                        })
                        .sum::<usize>()
            }
            Self::SessionAttached { .. } => named_field_overhead(1),
            Self::SessionState {
                title,
                selected_model,
                working_dir,
                turns,
                active_tool_groups,
                reasoning_effort,
                status,
                reasoning_capability,
                ..
            } => {
                named_field_overhead(13)
                    + option_str_len(title)
                    + option_str_len(selected_model)
                    + option_str_len(working_dir)
                    + option_str_len(reasoning_effort)
                    + active_tool_groups.iter().map(String::len).sum::<usize>()
                    + turns
                        .iter()
                        .map(|(id, turn)| 8 + *id as usize + turn.approx_size())
                        .sum::<usize>()
                    + session_status_size(status)
                    + reasoning_capability
                        .as_ref()
                        .map_or(0, reasoning_capability_size)
            }
            Self::TurnAppended { turn, .. } => named_field_overhead(3) + turn.approx_size(),
            Self::SessionStatusChanged { status, .. } => {
                named_field_overhead(3) + session_status_size(status)
            }
            Self::SessionFailed {
                operation, error, ..
            } => named_field_overhead(3) + operation.len() + error.len(),
            Self::Started { .. } => named_field_overhead(4),
            Self::ToolCallStarted {
                call_id,
                tool_name,
                arguments_json,
                invocation_description,
                ..
            } => {
                named_field_overhead(6)
                    + call_id.len()
                    + tool_name.len()
                    + arguments_json.len()
                    + invocation_description.len()
            }
            Self::ToolCallFinished {
                call_id, tool_name, ..
            } => named_field_overhead(4) + call_id.len() + tool_name.len(),
            Self::ToolResultChunk { call_id, data, .. } => {
                named_field_overhead(4) + call_id.len() + data.len()
            }
            Self::ToolCallFailed {
                call_id,
                tool_name,
                error,
                ..
            } => named_field_overhead(5) + call_id.len() + tool_name.len() + error.len(),
            Self::TokenUsageUpdate { .. } => named_field_overhead(4),
            Self::LiveOutputTokenCount { .. } => named_field_overhead(3),
            Self::OutputChunk { stream, data, .. } => {
                let stream_len = match stream {
                    OutputStream::Answer | OutputStream::Reasoning => 4,
                };
                named_field_overhead(4) + stream_len + data.len()
            }
            Self::Done { .. } => named_field_overhead(4),
            Self::Failed { error, .. } => named_field_overhead(3) + error.len(),
            Self::Cancelled { .. } => named_field_overhead(2),
            Self::Pong => OVERHEAD,
            Self::Models {
                models,
                selected_model,
            } => {
                named_field_overhead(2)
                    + models.iter().map(String::len).sum::<usize>()
                    + option_str_len(selected_model)
            }
            Self::ModelsFailed { error } => named_field_overhead(1) + error.len(),
            // providers/models are usize COUNTS (fixed-size scalars), covered
            // by the per-field allowance.
            Self::ModelsRefreshed { .. } => named_field_overhead(3),
            Self::ModelsRefreshFailed { error } => named_field_overhead(1) + error.len(),
            Self::CatalogUpdated { providers } => {
                named_field_overhead(1)
                    + providers
                        .iter()
                        .map(|p| 32 + p.slug.len() + p.display_name.len())
                        .sum::<usize>()
            }
            Self::ModelSelected {
                model,
                reasoning_capability,
                ..
            } => {
                named_field_overhead(2)
                    + model.len()
                    + reasoning_capability
                        .as_ref()
                        .map_or(0, reasoning_capability_size)
            }
            Self::ModelSelectionFailed { model, error, .. } => {
                named_field_overhead(3) + model.len() + error.len()
            }
            Self::Unlocked | Self::Locked | Self::ShuttingDown | Self::Evicted => OVERHEAD,
            Self::LockedError { error } => named_field_overhead(1) + error.len(),
            Self::CredentialAdded { service } => named_field_overhead(1) + service.len(),
            Self::CredentialAddFailed { service, error } => {
                named_field_overhead(2) + service.len() + error.len()
            }
            Self::CredentialRemoved { service } => named_field_overhead(1) + service.len(),
            Self::CredentialRemoveFailed { service, error } => {
                named_field_overhead(2) + service.len() + error.len()
            }
            Self::SessionDeleted { .. } => named_field_overhead(1),
            Self::SessionDeleteFailed { error, .. } => named_field_overhead(2) + error.len(),
            Self::TurnsUndone { turn_ids, .. } => named_field_overhead(2) + turn_ids.len() * 4,
            Self::TurnsRedone { turns, .. } => {
                named_field_overhead(2)
                    + turns
                        .iter()
                        .map(|(id, turn)| 8 + *id as usize + turn.approx_size())
                        .sum::<usize>()
            }
            Self::Credential { service, key } => {
                named_field_overhead(2) + service.len() + option_str_len(key)
            }
            Self::AccountAdded { name } => named_field_overhead(1) + name.len(),
            Self::AccountAddFailed { name, error } => {
                named_field_overhead(2) + name.len() + error.len()
            }
            Self::AccountRemoved { name } => named_field_overhead(1) + name.len(),
            Self::AccountRemoveFailed { name, error } => {
                named_field_overhead(2) + name.len() + error.len()
            }
            Self::Accounts { accounts } => {
                named_field_overhead(1)
                    + accounts
                        .iter()
                        .map(|a| 48 + a.name.len() + a.provider.len())
                        .sum::<usize>()
            }
            Self::AccountListFailed { error } => named_field_overhead(1) + error.len(),
            Self::SessionAccountSet { account, .. } => named_field_overhead(2) + account.len(),
            Self::ContextWindowResolved { .. } => named_field_overhead(2),
            Self::SessionWorkingDirSet { path, .. } => {
                named_field_overhead(2) + option_str_len(path)
            }
            Self::SessionTitleSet { title, .. } => named_field_overhead(2) + title.len(),
            Self::ReasoningEffortSet { effort, .. } => named_field_overhead(2) + effort.len(),
            Self::ReasoningEffortSetFailed { effort, error, .. } => {
                named_field_overhead(3) + effort.len() + error.len()
            }
        }
    }
}
