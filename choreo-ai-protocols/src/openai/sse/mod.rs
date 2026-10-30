//! Server-sent-events parsing for the `OpenAI` adapters.
//!
//! Two layers, one concern each:
//!
//! - [`reader`] — the format-agnostic frame reader ([`SseReader`]): it turns a
//!   byte stream into one `data:` payload per event and bounds how many bytes
//!   one event may accumulate, knowing nothing about what any payload means.
//! - [`responses`] — the Responses API's event vocabulary
//!   ([`ResponsesStreamEvent`], [`CallRef`]) and the parser that maps a payload
//!   onto it.
//!
//! `openai/mod.rs` names these items through this module, so the split stays
//! invisible to every consumer.

mod reader;
mod responses;

pub(crate) use reader::SseReader;
#[cfg(test)]
pub(crate) use reader::build_sse_event;
#[cfg(test)]
pub(crate) use responses::extract_responses_text_delta;
pub(crate) use responses::{CallRef, ResponsesStreamEvent, parse_responses_stream_event};
