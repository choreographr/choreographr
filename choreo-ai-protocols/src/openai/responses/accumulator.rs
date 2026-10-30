//! The streamed tool-call accumulator: assembling one call from the events a
//! Responses stream names it across.
//!
//! A streamed call arrives in two halves. Its `function_call` **output item**
//! (`response.output_item.added` / `.done`) carries the identity — the
//! function's name, the `call_id` the rebuilt request echoes back, the
//! provider's `caller` attribution — while `response.function_call_arguments.*`
//! carries the arguments and names nothing but that item. [`AccCalls`] owns the
//! join: it holds one [`AccCall`] per call under the identity the stream gave
//! it, folds every later event of that call into the same slot whichever order
//! the two halves arrive in, and bounds how many calls one turn may open. What
//! a finished turn does with the result — the truncated-call and unnamed-call
//! verdicts — belongs to the shared assembler (see [`super::turn`]), which is
//! why this module stops at [`FinishedCalls`].

use std::collections::HashMap;
use std::io;

use tracing::{trace, warn};

use super::super::{CallRef, OpenAiError, ResponsesStreamEvent};
use crate::shared::MAX_TOOL_CALLS;
use crate::types::{CallerInfo, ChatToolCall};

/// One function call the stream is assembling, held under the identity the
/// stream named it with (its output item's id, or its call id).
///
/// # What each event contributes
///
/// - `response.output_item.*` ([`ResponsesStreamEvent::FunctionCallItem`]) —
///   the call's *identity*: the function's name, which no other event on the
///   wire carries, plus the `call_id` the request echoes back. The item also
///   carries its own `arguments` copy, complete on `output_item.done`; on
///   `output_item.added` it is empty.
/// - `response.function_call_arguments.delta` — a chunk of JSON to **append**
///   to the accumulator (`arguments.push_str(&delta)`).
/// - `response.function_call_arguments.done` — the **complete** final arguments
///   string. When received, `arguments` is **replaced** (not appended),
///   overwriting any partial accumulation from prior delta events.
///
/// That matches the `OpenAI` spec: the done event carries the full final
/// arguments JSON, while delta events carry incremental fragments. The item's
/// own copy is held to the same rule, minus the empty one — see
/// [`AccCalls::absorb`].
pub(super) struct AccCall {
    /// The provider's `call_id`; the wire may omit it, in which case the
    /// request answers the call by whichever identity it did give.
    pub(super) call_id: Option<String>,
    /// The function the model asked for, named by the call's output item.
    pub(super) name: Option<String>,
    pub(super) arguments: String,
    /// The provider's `caller` attribution for this call, captured from its
    /// `function_call` output item. Kept so the finished call carries it back
    /// onto the rebuilt request: the API needs the same value on the
    /// `function_call_output` that answers the call to resume the generated
    /// program.
    pub(super) caller: Option<CallerInfo>,
    /// Monotonically increasing index assigned to this tool call, used to
    /// preserve insertion order since the Responses API does not provide
    /// server-side indices like Chat Completions does.
    pub(super) index: u32,
}

impl AccCall {
    pub(super) fn new(index: u32) -> Self {
        Self {
            call_id: None,
            name: None,
            arguments: String::new(),
            caller: None,
            index,
        }
    }
}

/// The streamed tool calls of one turn, keyed by the identity the stream named
/// each call with — its output item's id, falling back to its call id.
///
/// The Responses API assigns no server-side index, so the accumulator mints one
/// per first-seen call to keep the model's call order. The map is the call-count
/// bound: a *new* identity past [`MAX_TOOL_CALLS`] is rejected, while a later
/// event for an already-known call is always accepted — a full map must not
/// turn a legal continuation into an error.
#[derive(Default)]
pub(super) struct AccCalls {
    by_id: HashMap<String, AccCall>,
    next_index: u32,
}

impl AccCalls {
    /// The accumulator slot for one call identity, minting one when this is the
    /// first time the stream names it.
    ///
    /// `identity` is the call's own identity, never its `call_id` as such: an
    /// output item's id when an event carries one, otherwise the `call_id` a
    /// gateway-named stream gives. Both name the same slot for the same call.
    pub(super) fn entry(&mut self, identity: String) -> Result<&mut AccCall, OpenAiError> {
        if !self.by_id.contains_key(&identity) && self.by_id.len() >= MAX_TOOL_CALLS {
            return Err(OpenAiError::Io(io::Error::other(format!(
                "too many tool calls (max {MAX_TOOL_CALLS})"
            ))));
        }
        let next_index = &mut self.next_index;
        Ok(self.by_id.entry(identity).or_insert_with(|| {
            let index = *next_index;
            *next_index = next_index.saturating_add(1);
            AccCall::new(index)
        }))
    }

    /// The slot one event names, minting it the first time the stream mentions
    /// the call.
    ///
    /// An output-item id keys its own slot, because that is the join the
    /// documented argument events use. A bare call id is resolved against the
    /// slots already minted — a stream that sends output items *and* names only
    /// call ids on its argument events must still land on the call's one slot,
    /// or the same tool would be run twice — and only keys a new slot when
    /// nothing claims it.
    fn slot(&mut self, call: &CallRef) -> Result<&mut AccCall, OpenAiError> {
        let key = match call {
            CallRef::Item(item_id) => item_id.clone(),
            CallRef::Call(call_id) => self
                .by_id
                .iter()
                .find(|(_, acc)| acc.call_id.as_deref() == Some(call_id.as_str()))
                .map_or_else(|| call_id.clone(), |(key, _)| key.clone()),
        };
        self.entry(key)
    }

    /// The accumulated calls in the model's own order.
    ///
    /// Each entry keeps the identity its call was filed under — its output
    /// item's id, or the bare call id a gateway-named stream gave — which is
    /// what answers the call when the provider never reported a `call_id`.
    pub(super) fn into_ordered(self) -> Vec<(String, AccCall)> {
        let mut sorted: Vec<(String, AccCall)> = self.by_id.into_iter().collect();
        sorted.sort_by_key(|(_, acc)| acc.index);
        sorted
    }

    /// Apply one call-related stream event to the accumulator.
    ///
    /// The three call events are handled here rather than in the stream loop so
    /// the accumulation rules (append a delta, replace with `done`, name and
    /// attribute the call from its item) are exercisable without a socket.
    pub(super) fn absorb(&mut self, event: &ResponsesStreamEvent) -> Result<(), OpenAiError> {
        match event {
            ResponsesStreamEvent::FunctionCallItem {
                item_id,
                call_id,
                name,
                arguments,
                caller,
            } => {
                // The item is where a streamed call is identified: the argument
                // events name nothing but the item they belong to. An item that
                // names neither id can't be answered by any
                // `function_call_output`, so there is nothing to accumulate.
                let Some(call) = CallRef::of(item_id.as_deref(), call_id.as_deref()) else {
                    return Ok(());
                };
                // An argument event that named only the call id may already have
                // opened this call (nothing on the wire fixes the order of the
                // two events). That slot is the same call, so re-key it to the
                // item's id rather than minting a second one — which would run
                // the tool twice and count a phantom unnamed call.
                if let CallRef::Item(item_id) = &call
                    && let Some(call_id) = call_id.as_deref()
                    && !self.by_id.contains_key(item_id.as_str())
                    && let Some(acc) = self.by_id.remove(call_id)
                {
                    self.by_id.insert(item_id.clone(), acc);
                }
                let entry = self.slot(&call)?;
                if let Some(call_id) = call_id.as_deref() {
                    entry.call_id = Some(call_id.to_string());
                }
                if let Some(name) = name.as_deref() {
                    entry.name = Some(name.to_string());
                }
                // The item carries its own copy of the arguments — complete on
                // `output_item.done` — so it replaces whatever the deltas built.
                // The empty copy on `output_item.added` is not a value, and must
                // not wipe arguments that already arrived.
                if !arguments.is_empty() {
                    entry.arguments.clone_from(arguments);
                }
                if caller.is_some() {
                    entry.caller.clone_from(caller);
                }
                trace!(
                    tool_name = ?entry.name,
                    args_len = entry.arguments.len(),
                    "openai responses: function call item",
                );
            }
            ResponsesStreamEvent::FunctionCallArgumentsDelta { call, delta } => {
                // `call` is provided by the trusted API server — no validation
                // needed; it is used directly as the accumulator key for this
                // specific tool call.
                self.slot(call)?.arguments.push_str(delta);
            }
            ResponsesStreamEvent::FunctionCallArgumentsDone {
                call,
                arguments,
                name,
            } => {
                let entry = self.slot(call)?;
                // `arguments` is the complete final JSON string (not a delta), so
                // it replaces any accumulated partial value.
                entry.arguments.clone_from(arguments);
                // A gateway that relays no output items may name the function on
                // the arguments event, which the documented schema does not
                // carry. Read it as a fallback only: an item's name is the
                // stream's own statement of the call and always wins.
                if entry.name.is_none()
                    && let Some(name) = name.as_deref().filter(|name| !name.is_empty())
                {
                    entry.name = Some(name.to_string());
                }
            }
            // Not a call event: the stream loop routes only the three variants
            // above here.
            _ => {}
        }
        Ok(())
    }

    /// The finished tool calls in the model's own order, plus how many calls the
    /// stream mentioned without ever naming — a call with no function name can't
    /// be run, so it is counted for the turn's verdict rather than invented.
    pub(super) fn finish(self) -> FinishedCalls {
        let ordered = self.into_ordered();
        let mentioned = ordered.len();
        let calls: Vec<ChatToolCall> = ordered
            .into_iter()
            .filter_map(|(identity, acc)| {
                let name = acc.name.as_deref().filter(|n| !n.is_empty())?;
                Some(ChatToolCall {
                    // The request answers the call by `call_id`; a stream that
                    // never named one is answered by the identity it did give.
                    id: acc.call_id.unwrap_or(identity),
                    name: name.to_string(),
                    arguments_json: acc.arguments,
                    caller: acc.caller,
                })
            })
            .collect();
        let unnamed = mentioned - calls.len();
        if unnamed > 0 {
            warn!(
                dropped = unnamed,
                "the provider mentioned function call(s) without naming them \
                 (the stream carried no function_call output item and no name \
                 on the arguments event)"
            );
        }
        FinishedCalls { calls, unnamed }
    }
}

/// What one streamed turn's call accumulation produced.
#[derive(Debug)]
pub(super) struct FinishedCalls {
    /// The calls the model made, in its own order.
    pub(super) calls: Vec<ChatToolCall>,
    /// How many calls the stream mentioned without naming the function. They
    /// cannot be run, so the turn has to account for them rather than reporting
    /// an answer that silently omits them.
    pub(super) unnamed: usize,
}
