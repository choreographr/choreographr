//! Session-image resolution: the `DaemonCommand::GetSessionImage` handler and
//! its pure resolution logic.
//!
//! These are the `impl DaemonState` methods that turn a [`SessionImageSelector`]
//! into the resolved image bytes (`ResolvedImage`) a tool thread can feed to an
//! image-editing provider. They live in a child module (same pattern as
//! `daemon/image_provider.rs`) so `daemon.rs` stays focused on core command
//! handling; the handler is `pub(super)` because `handle_command` in the parent
//! dispatches the corresponding `DaemonCommand` variant here.
//!
//! Resolution reads the session's persisted turns (the byte-less blobs plus
//! their re-attached image bytes) to find the target attachment's slot and MIME
//! type, then reads the raw bytes with [`crate::db::read_attachment`]. The
//! command loop is the SOLE reader of session state on a tool's behalf — the
//! tool never touches turns or the attachment store directly, so no session
//! state is shared across threads (AGENTS.md "Thread Communication").
//!
//! As a CHILD of `crate::daemon`, this module reaches the parent's private
//! items via `use super::…`.

use super::{DaemonState, info, warn};
use choreo_proto::{ImageKey, Turn};
use thiserror::Error;

/// Which image already stored in a session an `edit_image` input refers to.
///
/// This is the parsed, typed form of an `edit_image` `images` entry that begins
/// with `@` (the tool parses the `@…` grammar and sends the selector in a
/// [`super::DaemonCommand::GetSessionImage`]; a non-`@` entry stays a
/// filesystem path and never becomes a selector).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionImageSelector {
    /// The most recent image attachment in the session: the highest-`turn_id`
    /// turn that has an attachment, then the last attachment within it
    /// (displayed images take precedence over tool-result images).
    Last,
    /// A displayed image: the `index`-th entry of the turn's append-only
    /// `displayed_images` list (the same positional index [`ImageKey::Displayed`]
    /// carries).
    Display {
        /// The turn id the displayed image belongs to.
        turn: u32,
        /// The zero-based position in that turn's `displayed_images`.
        index: u32,
    },
    /// A tool-result vision image (e.g. a `read_image` result), keyed by the
    /// tool call id that produced it.
    Result {
        /// The turn id the tool result belongs to.
        turn: u32,
        /// The `call_id` of the tool call whose result carries the image.
        call_id: String,
    },
}

impl std::fmt::Display for SessionImageSelector {
    /// Render the selector in the same `@…` grammar the tool parses, so log
    /// lines and error messages name the reference exactly as the model wrote
    /// it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Last => f.write_str("@last"),
            Self::Display { turn, index } => write!(f, "@display:{turn}:{index}"),
            Self::Result { turn, call_id } => write!(f, "@result:{turn}:{call_id}"),
        }
    }
}

/// A resolved session image: its MIME type and raw (already-normalized) bytes.
///
/// The bytes are exactly what was persisted when the image was produced
/// (normalized to a provider-allowlisted format at that point), so the tool
/// feeds them straight into an [`ImageInput`](choreo_ai_protocols::images::ImageInput)
/// without re-normalizing.
#[derive(Debug, Clone)]
pub struct ResolvedImage {
    /// The stored image's MIME type (e.g. `image/png`).
    pub mime_type: String,
    /// The stored image's raw encoded bytes.
    pub data: Vec<u8>,
}

/// Why resolving a session image failed.
#[derive(Debug, Clone, Error)]
pub enum SessionImageError {
    /// `@last` was requested but the session holds no image attachment at all.
    #[error("this session has no images to reference with `{selector}`")]
    NoImages {
        /// The selector that named no image (always `@last` in practice).
        selector: String,
    },
    /// An explicit `@display:<turn>:<index>` named a turn or index with no
    /// displayed image.
    #[error("no displayed image at turn {turn}, index {index}")]
    DisplayNotFound {
        /// The turn id that was addressed.
        turn: u32,
        /// The display index that was addressed.
        index: u32,
    },
    /// An explicit `@result:<turn>:<call_id>` named a turn or call with no
    /// vision image.
    #[error("no image for tool call `{call_id}` at turn {turn}")]
    ResultNotFound {
        /// The turn id that was addressed.
        turn: u32,
        /// The tool call id that was addressed.
        call_id: String,
    },
    /// The attachment's metadata exists but its bytes are not in the store
    /// (e.g. an image that was never split out, or was deleted).
    #[error("the referenced image's bytes are no longer stored in this session")]
    BytesMissing,
    /// A genuine storage failure reading the session's turns or attachment.
    #[error("failed to read session images: {0}")]
    Storage(String),
}

impl DaemonState {
    /// Resolve a `GetSessionImage` command and reply over the caller's crossbeam
    /// channel.
    ///
    /// Every path replies over the caller's crossbeam channel — never the
    /// broadcast machinery — and logs the outcome so a bad reference leaves a
    /// trace in the daemon log. The image BYTES are never logged; only the
    /// selector, MIME type, and byte length.
    pub(super) fn handle_get_session_image(
        &self,
        session_id: u64,
        selector: &SessionImageSelector,
        reply: &crossbeam_channel::Sender<Result<ResolvedImage, SessionImageError>>,
    ) {
        let result = self.resolve_session_image(session_id, selector);
        match &result {
            Ok(image) => {
                info!(
                    session_id,
                    selector = %selector,
                    mime = %image.mime_type,
                    bytes = image.data.len(),
                    "resolved in-session image"
                );
            }
            Err(err) => {
                warn!(
                    session_id,
                    selector = %selector,
                    error = %err,
                    "in-session image resolution failed"
                );
            }
        }
        // Best-effort send: a dropped receiver (tool cancelled mid-call) must
        // not panic the command loop.
        let _ = reply.send(result);
    }

    /// Pure resolution logic for [`Self::handle_get_session_image`], split out
    /// so the selector→slot mapping is unit-testable through the real command
    /// path without threading a channel through the test.
    ///
    /// The session's persisted turns are the source of the selector→slot
    /// mapping: a displayed image's MIME comes from the turn's
    /// `displayed_images[i].metadata` and a tool-result image's from its
    /// `ImageReference`. The raw bytes are then read in a single `get` from the
    /// attachment store ([`crate::db::read_attachment`]), so the already-
    /// normalized bytes are handed back unchanged.
    pub(super) fn resolve_session_image(
        &self,
        session_id: u64,
        selector: &SessionImageSelector,
    ) -> Result<ResolvedImage, SessionImageError> {
        let turns = crate::db::read_turns(&self.db, session_id)
            .map_err(|e| SessionImageError::Storage(e.to_string()))?;
        let (turn_id, key, mime_type) = resolve_slot(&turns, selector)?;
        let data = crate::db::read_attachment(&self.db, session_id, turn_id, &key)
            .map_err(|e| SessionImageError::Storage(e.to_string()))?
            .ok_or(SessionImageError::BytesMissing)?;
        Ok(ResolvedImage { mime_type, data })
    }
}

/// Map a selector to `(turn_id, attachment key, MIME type)` against a session's
/// decoded turns. Pure, so it is unit-testable in isolation.
fn resolve_slot(
    turns: &[(u32, Turn)],
    selector: &SessionImageSelector,
) -> Result<(u32, ImageKey, String), SessionImageError> {
    match selector {
        SessionImageSelector::Last => {
            last_attachment(turns).ok_or_else(|| SessionImageError::NoImages {
                selector: selector.to_string(),
            })
        }
        SessionImageSelector::Display { turn, index } => {
            let image = turns
                .iter()
                .find(|(id, _)| id == turn)
                .and_then(|(_, t)| t.displayed_images.get(*index as usize))
                .ok_or(SessionImageError::DisplayNotFound {
                    turn: *turn,
                    index: *index,
                })?;
            Ok((
                *turn,
                ImageKey::Displayed { index: *index },
                image.metadata.mime_type.clone(),
            ))
        }
        SessionImageSelector::Result { turn, call_id } => {
            let image = turns
                .iter()
                .find(|(id, _)| id == turn)
                .and_then(|(_, t)| t.tool_results.iter().find(|r| &r.call_id == call_id))
                .and_then(|r| r.image.as_ref())
                .ok_or_else(|| SessionImageError::ResultNotFound {
                    turn: *turn,
                    call_id: call_id.clone(),
                })?;
            Ok((
                *turn,
                ImageKey::ToolResult {
                    call_id: call_id.clone(),
                },
                image.mime_type.clone(),
            ))
        }
    }
}

/// The most recent attachment in a session's turns, as
/// `(turn_id, key, MIME type)`, or `None` when no turn holds an image.
///
/// Turns arrive sorted by ascending `turn_id`, so the search walks them in
/// reverse for the highest turn with an attachment. Within that turn the last
/// DISPLAYED image (highest index) wins; a turn with no displayed image falls
/// back to its last tool-result vision image. Only images whose stored bytes
/// were re-attached (`data` non-empty) count — an attachment row that was never
/// written is not an image to reference.
fn last_attachment(turns: &[(u32, Turn)]) -> Option<(u32, ImageKey, String)> {
    for (turn_id, turn) in turns.iter().rev() {
        if let Some((index, image)) = turn
            .displayed_images
            .iter()
            .enumerate()
            .rev()
            .find(|(_, img)| !img.data.is_empty())
        {
            // usize→u32: the display index is bounded far below u32 in practice;
            // saturating keeps the value total at the theoretical cap (matching
            // the slot-naming contract in `db::attachments`).
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            return Some((
                *turn_id,
                ImageKey::Displayed { index },
                image.metadata.mime_type.clone(),
            ));
        }
        if let Some(result) = turn
            .tool_results
            .iter()
            .rev()
            .find(|r| r.image.as_ref().is_some_and(|img| !img.data.is_empty()))
            && let Some(image) = &result.image
        {
            return Some((
                *turn_id,
                ImageKey::ToolResult {
                    call_id: result.call_id.clone(),
                },
                image.mime_type.clone(),
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    // The glob above only reaches this module's own imports; the test-only
    // helpers come from further up the tree and must be named explicitly.
    use crate::daemon::tests::make_daemon_state;
    use choreo_proto::{ImageMetadata, ImageReference, ToolResultRecord};

    /// Build a displayed image record whose bytes are already present (as a
    /// freshly-produced turn would carry them before the write/read split).
    fn displayed(mime: &str) -> choreo_proto::DisplayedImageRecord {
        choreo_proto::DisplayedImageRecord {
            metadata: ImageMetadata {
                mime_type: mime.to_string(),
                width: 4,
                height: 3,
                byte_len: 4,
                alt: None,
            },
            data: vec![1, 2, 3, 4],
            tool_call_id: None,
        }
    }

    /// Build a tool result carrying a vision image.
    fn result_with_image(call_id: &str, mime: &str) -> ToolResultRecord {
        ToolResultRecord {
            call_id: call_id.to_string(),
            name: "read_image".to_string(),
            content: String::new(),
            is_error: false,
            invocation_description: String::new(),
            image: Some(ImageReference {
                path: "x.png".to_string(),
                mime_type: mime.to_string(),
                width: 4,
                height: 3,
                data: vec![9, 8, 7],
            }),
        }
    }

    /// A turn with no images, used to pad turn ids in fixtures.
    fn empty_turn() -> Turn {
        Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        }
    }

    /// Send a `GetSessionImage` command and wait for the crossbeam reply (the
    /// same channel shape the tool thread uses in production).
    fn send_get_session_image(
        state: &mut DaemonState,
        session_id: u64,
        selector: SessionImageSelector,
    ) -> Result<ResolvedImage, SessionImageError> {
        let (reply, rx) = crossbeam_channel::unbounded();
        state.handle_command(super::super::DaemonCommand::GetSessionImage {
            session_id,
            selector,
            reply,
        });
        rx.recv().unwrap()
    }

    #[test]
    fn last_selector_returns_the_most_recent_attachment() {
        let (mut state, _rx) = make_daemon_state();
        let sid = 7;
        // Turn 0: an attachment whose bytes were never split out (empty) — it
        // must be skipped as "not an image". Turn 1: a genuine displayed image.
        let mut t0 = empty_turn();
        t0.displayed_images.push(displayed("image/png"));
        t0.displayed_images[0].data.clear();
        crate::db::write_turn(&state.db, sid, 0, &t0).unwrap();
        let mut t1 = empty_turn();
        t1.displayed_images.push(displayed("image/png"));
        crate::db::write_turn(&state.db, sid, 1, &t1).unwrap();

        let image = send_get_session_image(&mut state, sid, SessionImageSelector::Last).unwrap();
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(image.data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn last_selector_prefers_a_tool_result_when_no_displayed_image() {
        let (mut state, _rx) = make_daemon_state();
        let sid = 7;
        // A read_image result on turn 2 is the only attachment.
        let mut t2 = empty_turn();
        t2.tool_results
            .push(result_with_image("call_abc", "image/jpeg"));
        crate::db::write_turn(&state.db, sid, 2, &t2).unwrap();

        let image = send_get_session_image(&mut state, sid, SessionImageSelector::Last).unwrap();
        assert_eq!(image.mime_type, "image/jpeg");
        assert_eq!(image.data, vec![9, 8, 7]);
    }

    #[test]
    fn last_selector_with_no_images_errors_cleanly() {
        let (mut state, _rx) = make_daemon_state();
        let sid = 7;
        // A turn exists but carries no image at all.
        crate::db::write_turn(&state.db, sid, 0, &empty_turn()).unwrap();

        let err = send_get_session_image(&mut state, sid, SessionImageSelector::Last).unwrap_err();
        assert!(
            matches!(&err, SessionImageError::NoImages { selector } if selector == "@last")
                && err.to_string().contains("no images"),
            "clean no-images error expected, got: {err}"
        );
    }

    #[test]
    fn explicit_display_selector_resolves_by_turn_and_index() {
        let (mut state, _rx) = make_daemon_state();
        let sid = 7;
        let mut turn = empty_turn();
        turn.displayed_images.push(displayed("image/png"));
        crate::db::write_turn(&state.db, sid, 4, &turn).unwrap();

        let image = send_get_session_image(
            &mut state,
            sid,
            SessionImageSelector::Display { turn: 4, index: 0 },
        )
        .unwrap();
        assert_eq!(image.data, vec![1, 2, 3, 4]);

        let err = send_get_session_image(
            &mut state,
            sid,
            SessionImageSelector::Display { turn: 4, index: 3 },
        )
        .unwrap_err();
        assert!(
            matches!(err, SessionImageError::DisplayNotFound { .. }),
            "{err}"
        );
    }

    #[test]
    fn explicit_result_selector_resolves_by_turn_and_call_id() {
        let (mut state, _rx) = make_daemon_state();
        let sid = 7;
        let mut turn = empty_turn();
        turn.tool_results
            .push(result_with_image("call_abc", "image/png"));
        crate::db::write_turn(&state.db, sid, 5, &turn).unwrap();

        let image = send_get_session_image(
            &mut state,
            sid,
            SessionImageSelector::Result {
                turn: 5,
                call_id: "call_abc".to_string(),
            },
        )
        .unwrap();
        assert_eq!(image.data, vec![9, 8, 7]);

        let err = send_get_session_image(
            &mut state,
            sid,
            SessionImageSelector::Result {
                turn: 5,
                call_id: "call_missing".to_string(),
            },
        )
        .unwrap_err();
        assert!(
            matches!(err, SessionImageError::ResultNotFound { .. }),
            "{err}"
        );
    }

    #[test]
    fn selector_display_round_trips_its_grammar() {
        assert_eq!(SessionImageSelector::Last.to_string(), "@last");
        assert_eq!(
            SessionImageSelector::Display { turn: 2, index: 1 }.to_string(),
            "@display:2:1"
        );
        assert_eq!(
            SessionImageSelector::Result {
                turn: 3,
                call_id: "call_x".to_string()
            }
            .to_string(),
            "@result:3:call_x"
        );
    }
}
