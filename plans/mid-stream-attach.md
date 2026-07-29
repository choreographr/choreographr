# Mid-Stream Session Attach: Full Content Recovery

## Problem

When the TUI switches to a session that is already streaming (has an active
request), the turn appears blank until the request finishes. The minimal fix
(re-sending `Started` on attach, merged in commit `…`) ensures that *new*
streaming chunks are routed correctly, but **content that streamed before the
TUI attached is permanently lost on that client**.

The turn in the main session state has `assistant_text: None` because the
worker thread owns the turn and only syncs back on `RequestFinished`. The
TUI receives the empty turn via `SessionState`, sees it in `visible_turn_ids`,
but the `assistant_text` field never gets populated retroactively — only new
`OutputChunk` messages fill it via `stream_chunk()`.

## Goal

A late-attaching TUI should see the streaming content accumulated so far
*as if it had been attached from the start*. Ideally the TUI could also
"peek" at sessions it is not currently viewing, but the primary goal is
seamless switching.

---

## Architecture Review

### Thread topology relevant to this change

```
Session main thread
├── owns SessionState (subscribers, active_requests, turns, config)
├── processes SessionCommands sequentially
└── broadcasts DaemonMessages to all subscribers

Worker thread (spawned per request)
├── owns a snapshot copy of SessionState (forked on Start)
├── mutates turns (assistant_text, tool_calls, tool_results)
├── broadcasts streaming chunks via SessionCommand::Broadcast
└── on finish: sends snapshot back via SessionCommand::RequestFinished
```

### Key observation

The worker **already** broadcasts every mutation as it happens:
- `OutputChunk` → `SessionCommand::Broadcast` → `broadcast()` → all subscribers
- `TurnAppended` → same path (via `broadcast_turn_appended`)
- `ToolResultChunk` → same path

So the *current* subscriber set sees everything in real time. The problem is
that a subscriber joining mid-stream never saw the messages that already
flew past.

### What the worker owns that the main session thread doesn't

| Field | In main state? | In worker? | Synced on |
|---|---|---|---|
| `turn.assistant_text` | `None` (empty turn placeholder) | `Some("…")` growing | `RequestFinished` |
| `turn.assistant_reasoning` | `None` | `Some("…")` growing | `RequestFinished` |
| `turn.tool_calls` | `[]` | populated | `RequestFinished` |
| `turn.tool_results` | `[]` | populated | `RequestFinished` |
| `turn.token_usage` | `None` | `Some(…)` | `RequestFinished` |
| `turn.displayed_images` | `[]` | populated | `RequestFinished` |

The worker's turn is the **source of truth** for all streaming content.

---

## Approach 1: Periodic Worker Sync (Recommended)

The worker periodically publishes its current turn state back to the main
session thread, which updates `turns` and broadcasts a `TurnAppended` to all
subscribers.

### Protocol changes

Add a new `SessionCommand` variant:

```rust
enum SessionCommand {
    // … existing variants …
    /// Periodic heartbeat from the worker: replace the specified turn
    /// in the main session state and broadcast to subscribers.
    StreamingHeartbeat {
        turn_id: u32,
        turn: Turn,
    },
}
```

### Worker changes

At strategic points in the agent loop (after each `OutputChunk` batch, after
each `ToolResultChunk` batch, after each tool call finishes), the worker
sends a `StreamingHeartbeat` with the current turn snapshot:

```rust
// In run_agent_loop, after accumulating streaming content:
let _ = ctx.cmd_tx.send(SessionCommand::StreamingHeartbeat {
    turn_id: current_turn_id,
    turn: session.turns.get(&current_turn_id).cloned().unwrap_or_default(),
});
```

This is a `try_send` on a bounded channel — if the session main loop is
backed up, heartbeats are dropped. The *next* heartbeat carries cumulative
content, so dropped heartbeats are safe (same reasoning as existing
`OutputChunk` dropping).

### Session main loop changes

A new handler `handle_streaming_heartbeat`:

```rust
fn handle_streaming_heartbeat(
    turn_id: u32,
    turn: Turn,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    // Update the main session's turn with the worker's latest snapshot.
    // This is a replace — the worker has the complete picture.
    state.turns.insert(turn_id, turn.clone());
    // Broadcast to all current subscribers (including late joiners).
    broadcast(&mut state.subscribers, DaemonMessage::TurnAppended {
        turn_id,
        turn,
    });
    false
}
```

### Attach-time flow (with this change)

```
1. TUI sends AttachSession
2. Daemon sends Started (for each active request) ← existing fix
3. Daemon sends SessionState (with empty turn)
4. TUI populates request_to_turn map from Started messages
5. TUI receives SessionState, renders empty turn
6. Next heartbeat arrives from worker → TurnAppended with real content
7. TUI replaces the empty turn → user sees accumulated content
8. Ongoing streaming chunks continue to arrive and update the turn
9. Request finishes → TurnFinalized with final turn state
```

### Heartbeat frequency

Two strategies, not mutually exclusive:

**a) Time-based:** A timer in the worker fires every 200–500ms, sending the
current turn snapshot. Simple, ensures regular sync.

**b) Event-based:** After every `OutputChunk` / `ToolResultChunk` that
advances the content, send a heartbeat. This is reactive and doesn't waste
bandwidth when nothing is changing (e.g. waiting for a slow LLM response).

Recommendation: **b)** — emit a heartbeat after each `OutputChunk` batch
(which already accumulates multiple tokens in `SessionCommand::Broadcast`).
The overhead is one extra channel send per batch, and each heartbeat carries
the complete turn snapshot so dropped ones are harmless.

### Edge cases

**Worker finishes between attach and first heartbeat:**
The ordering is:
1. `SessionState` arrives (empty turn)
2. `RequestFinished` arrives → `turn_id` lookup fails in TUI's
   `request_to_turn` (already cleaned up by `handle_done`)
3. Or: `Done` + `TurnFinalized` arrive with the complete turn

The TUI's `handle_done` calls `mark_content_changed()` which does a full
rebuild from `turns`. The `TurnFinalized` inserts the complete turn. So this
case is already handled — the TUI will see the complete turn after `Done`.

**Rapid streaming (every chunk sends a heartbeat):**
The session main loop processes `SessionCommand`s sequentially. If the worker
is sending heartbeats after every chunk and the main loop falls behind,
heartbeats accumulate in the channel. This is manageable because:
- The channel is bounded (configurable, say 256 entries)
- Once full, `send()` blocks the worker → flow control
- Each heartbeat replaces the previous turn, so the main loop can skip
  intermediate heartbeats (the last one is the most current)

However, it's cleaner to merge: send one heartbeat per `OutputChunk` *batch*
(i.e. the same frequency at which `SessionCommand::Broadcast(OutputChunk)` is
already called — once per provider stream event). This keeps the heartbeat
rate identical to the existing chunk rate.

### Risk assessment

| Risk | Mitigation |
|---|---|
| Channel congestion from rapid heartbeats | Same bounded channel as `Broadcast`; heartbeats are cumulative drop-safe |
| Worker overhead from serializing turns | `Turn` is already cloned for `TurnAppended` broadcast; minimal additional cost |
| TUI confusion from stale heartbeat after `Done` | `Done` → `mark_content_changed()` → full rebuild; stale heartbeats' turn is replaced by the final `TurnFinalized` |
| Heartbeat arrives before `SessionState` | `handle_daemon_message` dispatches in order; `SessionState` is sent directly by the main thread before returning from `handle_attach`; heartbeat arrives after |

---

## Approach 2: On-Attach Pull (Alternative)

Instead of the worker pushing heartbeats, the attach handler pulls the
latest turn state from the worker on demand.

### Protocol changes

Add an `mpsc::Sender<Turn>` field to `ActiveRequest` that the worker stores
alongside `cancel_tx`:

```rust
pub(crate) struct ActiveRequest {
    pub(crate) cancel_tx: mpsc::Sender<()>,
    pub(crate) turn_id: u32,
    /// Channel to request a snapshot of the currently-streaming turn.
    /// The worker responds by sending the current Turn on this channel
    /// (one-shot), or None if the turn is no longer active.
    pub(crate) snapshot_tx: mpsc::Sender<Turn>,
}
```

### Attach-time flow

```
1. TUI sends AttachSession
2. handle_attach sends Started for each active request
3. handle_attach pulls current turn from worker via snapshot_tx
4. handle_attach embeds the snapshot into SessionState (or sends TurnAppended)
5. TUI gets full turn state immediately
```

### Problem

This is a synchronous request-response across threads — the worker must
interrupt its work to reply. If the worker is blocked in an LLM API call,
the attach stalls. Mitigating with a timeout means the attach succeeds but
without content, defeating the purpose.

This approach is more complex and less robust than Approach 1. **Not
recommended.**

---

## Approach 3: Background Session Subscription (Bonus — "Peeking")

The user's second question was whether the TUI can "see" streaming for
sessions it is not currently viewing, so it can jump in aware of progress.

This would require the TUI to subscribe to *all* session activity, not just
the attached session. The architecture already supports multiple subscribers
per session — the daemon's `summary_subscribers` is a global broadcast for
session metadata.

### Protocol changes

Add a `DaemonMessage::TurnActivity` event:

```rust
enum DaemonMessage {
    // … existing variants …
    /// Lightweight notification that a session has new streaming content.
    /// Emitted for all sessions with active requests, regardless of
    /// which session the client is attached to.
    TurnActivity {
        session_id: u64,
        status: SessionStatus,
        /// Truncated preview of the current assistant text (first 200 chars).
        preview: String,
        tool_name: Option<String>,
    },
}
```

Add a client opt-in:

```rust
enum ClientMessage {
    // … existing variants …
    /// Subscribe to TurnActivity for all sessions.
    SubscribeAllActivity,
    /// Unsubscribe from TurnActivity.
    UnsubscribeAllActivity,
}
```

### Daemon changes

Each session's `broadcast()` also sends `TurnActivity` to the daemon's
global summary subscriber set when the session has active requests. The TUI
subscribes to this on startup (or when entering the session manager).

### TUI changes

- A new "activity monitor" pane or status-line indicator showing recently
  active sessions
- Each entry: `session_id:48 → 🔧 reading file main.rs`
- Updated progressively as `TurnActivity` events arrive
- Click/cursor to jump to that session

### Risk assessment

| Risk | Mitigation |
|---|---|
| Flood of events for fast-streaming sessions | Throttle: emit `TurnActivity` at most once per 500ms per session |
| Privacy (content previews leak across sessions) | Only status + truncated preview; no full content |
| Complexity | Medium — new channel, new protocol messages, new UI element |

This is a separate feature from the mid-stream content recovery, but
architecturally compatible with Approach 1.

---

## Recommended Implementation Order

| Step | Description | Est. effort |
|---|---|---|
| 1 | Add `StreamingHeartbeat` variant to `SessionCommand` | 1 hour |
| 2 | Implement `handle_streaming_heartbeat` in `sessions.rs` | 1 hour |
| 3 | Add heartbeat emission in `run_agent_loop` (after `OutputChunk` / `ToolResultChunk` batches) | 2 hours |
| 4 | Verify TUI correctly handles `TurnAppended` for a turn it already knows about (it does — `insert_or_replace` + `mark_content_changed`) | 0 hours |
| 5 | Test: full workspace test suite; manual test with slow streaming model | 1 hour |
| 6 | (Optional) Implement `TurnActivity` + background peek | 3–5 hours |

Step 1–5 implements the "full content recovery" goal. Step 6 is the
separate "peeking" feature.

---

## Open Questions

1. **How should the TUI distinguish between an *initial* `TurnAppended` for
   a new turn (needs layout) and a *heartbeat* `TurnAppended` for an
   existing turn (needs re-render)?**  
   Currently `handle_turn_appended` calls `mark_content_changed()` (full
   rebuild). For heartbeat turns we could use `mark_streaming_changed()` and
   rely on the existing streaming-dirty fast path — but that path assumes
   `streaming_turn_index` is already set, which is true here because
   `Started` is sent before `SessionState`.

2. **Should heartbeat turns reset `streaming_dirty`?**  
   No — the streaming flag is already set by the initial `Started` +
   `OutputChunk` pipeline. The heartbeat is just another content update.

3. **Does the client's `TurnAppended` handler need a new code path for
   "replacing an existing turn with a more complete version"?**  
   No — `insert_or_replace` is already idempotent, and `mark_content_changed`
   triggers a full rebuild from `turns`, so the new content is picked up.
   For efficiency, the handler could detect "turn already exists with same
   turn_id" and call `mark_streaming_changed()` instead of
   `mark_content_changed()`, but correctness does not depend on this
   optimization.
