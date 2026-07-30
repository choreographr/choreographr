use crate::daemon::DaemonCommand;
use crate::sessions::MAX_TITLE_CHARS;
use crate::tools::context::ToolContext;
use crate::tools::{Tool, ToolExecError};
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::info;
use unicode_segmentation::UnicodeSegmentation;

// ── Args ───────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(crate) struct SetSessionTitleArgs {
    /// New title for the session.
    title: String,
}

// ── Result ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SetSessionTitleResult {
    /// The new title of the session.
    pub title: String,
}

// ── Execute ────────────────────────────────────────────────────────────────

fn execute_set_session_title(
    args: &SetSessionTitleArgs,
    _working_dir: Option<&Path>,
    ctx: Option<&ToolContext>,
) -> Result<SetSessionTitleResult, ToolExecError> {
    let ctx = ctx.ok_or_else(|| ToolExecError("no session context".into()))?;

    // Strip leading/trailing whitespace so that titles like "  " or
    // "  foo  " don't result in visually empty or oddly-padded labels.
    let title = args.title.trim();

    // Validate title length by grapheme clusters (user-perceived characters)
    // so that composed emoji and multi-byte scripts are counted fairly.
    let title_graphemes = title.graphemes(true).count();
    if title_graphemes > MAX_TITLE_CHARS {
        return Err(ToolExecError(format!(
            "session title too long: {title_graphemes} characters (max {MAX_TITLE_CHARS})",
        )));
    }

    info!(
        session_id = ctx.session_id,
        title_len = title_graphemes,
        title = %title,
        "setting session title",
    );

    // Route the title change through the daemon, which forwards it to the
    // session's main loop for in-memory update, broadcast, and persistence.
    ctx.daemon_tx
        .send(DaemonCommand::SetSessionTitle {
            session_id: ctx.session_id,
            title: title.to_owned(),
        })
        .map_err(|e| ToolExecError(format!("daemon communication failed: {e}")))?;

    Ok(SetSessionTitleResult {
        title: title.to_owned(),
    })
}

pub fn describe_invocation(args: &SetSessionTitleArgs) -> String {
    format!("Setting session title to '{}'.", args.title)
}

// ── Tool impl ──────────────────────────────────────────────────────────────

pub(crate) struct SetSessionTitle;

impl Tool for SetSessionTitle {
    type Args = SetSessionTitleArgs;
    type Return = SetSessionTitleResult;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "set_session_title"
    }

    fn group(&self) -> &'static str {
        "core"
    }

    fn description(&self) -> &'static str {
        "Set the display title for this session. Use this to give the session a meaningful name that will appear in session listings."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        describe_invocation(args)
    }

    fn return_string(ret: &Self::Return) -> String {
        format!("Session title changed to '{}'.", ret.title)
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        execute_set_session_title(&args, working_dir, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::context::ToolContext;
    use std::sync::Arc;

    /// Build a ToolContext with a mock daemon channel.
    /// Returns (context, sender, receiver) so the test can keep the
    /// receiver alive and verify messages.
    ///
    /// Uses `into_path()` on the tempdir to prevent the OS from removing
    /// the directory while the database (which holds an open file handle)
    /// is still in use. The directory leaks and is cleaned up by the OS
    /// temp-directory policy — acceptable for short-lived test helpers.
    fn test_context() -> (
        ToolContext,
        std::sync::mpsc::Sender<DaemonCommand>,
        std::sync::mpsc::Receiver<DaemonCommand>,
    ) {
        let (daemon_tx, daemon_rx) = std::sync::mpsc::channel::<DaemonCommand>();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.keep(); // Leak: prevent early cleanup of the temp directory.
        let db = Arc::new(redb::Database::create(db_path.join("test.redb")).unwrap());
        let ctx = ToolContext::new(42, db, daemon_tx.clone());
        (ctx, daemon_tx, daemon_rx)
    }

    #[test]
    fn execute_set_session_title_sends_daemon_command() {
        let (ctx, _daemon_tx, daemon_rx) = test_context();
        let args = SetSessionTitleArgs {
            title: "My Session".into(),
        };

        let result = execute_set_session_title(&args, None, Some(&ctx));
        assert!(result.is_ok());
        assert_eq!(result.unwrap().title, "My Session");

        // Verify a SetSessionTitle command was sent to the daemon.
        match daemon_rx.try_recv() {
            Ok(DaemonCommand::SetSessionTitle { session_id, title }) => {
                assert_eq!(session_id, 42);
                assert_eq!(title, "My Session");
            }
            Ok(other) => panic!(
                "expected SetSessionTitle, got something else: {:?}",
                std::mem::discriminant(&other)
            ),
            Err(e) => panic!("expected SetSessionTitle, got {e:?}"),
        }
    }

    #[test]
    fn execute_no_context_returns_error() {
        let args = SetSessionTitleArgs {
            title: "test".into(),
        };
        let result = execute_set_session_title(&args, None, None);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no session context")
        );
    }

    #[test]
    fn describe_invocation_includes_title() {
        let args = SetSessionTitleArgs {
            title: "My Session".into(),
        };
        let desc = describe_invocation(&args);
        assert_eq!(desc, "Setting session title to 'My Session'.");
    }

    #[test]
    fn return_string_formats_correctly() {
        let result = SetSessionTitleResult {
            title: "My Session".into(),
        };
        let s = SetSessionTitle::return_string(&result);
        assert_eq!(s, "Session title changed to 'My Session'.");
    }

    #[test]
    fn tool_schema_has_title_parameter() {
        let tool = SetSessionTitle;
        let schema = tool.schema();
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("title"));
        assert_eq!(props["title"]["type"], "string");
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "title"));
    }

    #[test]
    fn execute_title_too_long_returns_error() {
        let (ctx, _daemon_tx, _daemon_rx) = test_context();
        let title = "a".repeat(MAX_TITLE_CHARS + 1);
        let args = SetSessionTitleArgs {
            title: title.clone(),
        };
        let result = execute_set_session_title(&args, None, Some(&ctx));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too long"));
    }

    #[test]
    fn execute_title_at_max_length_succeeds() {
        let (ctx, _daemon_tx, daemon_rx) = test_context();
        let title = "a".repeat(MAX_TITLE_CHARS);
        let args = SetSessionTitleArgs {
            title: title.clone(),
        };
        let result = execute_set_session_title(&args, None, Some(&ctx));
        assert!(result.is_ok());
        assert_eq!(result.unwrap().title, title);

        // Verify a SetSessionTitle command was sent with the correct title.
        match daemon_rx.try_recv() {
            Ok(DaemonCommand::SetSessionTitle {
                session_id,
                title: sent_title,
            }) => {
                assert_eq!(session_id, 42);
                assert_eq!(sent_title, title);
            }
            Ok(other) => panic!(
                "expected SetSessionTitle, got something else: {:?}",
                std::mem::discriminant(&other)
            ),
            Err(e) => panic!("expected SetSessionTitle, got {e:?}"),
        }
    }

    #[test]
    fn execute_title_with_emoji_counts_graphemes_not_bytes() {
        let (ctx, _daemon_tx, daemon_rx) = test_context();
        // A string of 200 grapheme clusters where each is a multi-byte
        // emoji ("😀" = 1 grapheme, 1 char, 4 bytes).  Byte length =
        // 200 × 4 = 800 bytes — well past any byte-based limit, but
        // valid under our grapheme-cluster-based limit.
        let title = "😀".repeat(MAX_TITLE_CHARS);
        assert_eq!(title.graphemes(true).count(), MAX_TITLE_CHARS);
        assert!(
            title.len() > MAX_TITLE_CHARS,
            "multi-byte chars exceed byte limit"
        );
        let args = SetSessionTitleArgs {
            title: title.clone(),
        };
        let result = execute_set_session_title(&args, None, Some(&ctx));
        assert!(result.is_ok());
        let _ = daemon_rx.try_recv();
    }

    #[test]
    fn execute_title_trim_whitespace() {
        let (ctx, _daemon_tx, daemon_rx) = test_context();
        let args = SetSessionTitleArgs {
            title: "  spaced title  ".into(),
        };

        let result = execute_set_session_title(&args, None, Some(&ctx));
        assert!(result.is_ok());
        // The result should reflect the trimmed value.
        assert_eq!(result.unwrap().title, "spaced title");

        // The daemon should receive the trimmed title.
        match daemon_rx.try_recv() {
            Ok(DaemonCommand::SetSessionTitle { session_id, title }) => {
                assert_eq!(session_id, 42);
                assert_eq!(title, "spaced title");
            }
            Ok(other) => panic!(
                "expected SetSessionTitle, got something else: {:?}",
                std::mem::discriminant(&other)
            ),
            Err(e) => panic!("expected SetSessionTitle, got {e:?}"),
        }
    }

    #[test]
    fn execute_title_trim_rejects_whitespace_only_as_empty() {
        let (ctx, _daemon_tx, daemon_rx) = test_context();
        // After trimming, a whitespace-only title becomes the empty string.
        // The empty string is valid (0 graphemes ≤ MAX_TITLE_CHARS) and
        // is stored as Some("") rather than None.
        let args = SetSessionTitleArgs {
            title: "   ".into(),
        };

        let result = execute_set_session_title(&args, None, Some(&ctx));
        assert!(result.is_ok());
        assert_eq!(result.unwrap().title, "");

        match daemon_rx.try_recv() {
            Ok(DaemonCommand::SetSessionTitle { session_id, title }) => {
                assert_eq!(session_id, 42);
                assert_eq!(title, "");
            }
            Ok(other) => panic!(
                "expected SetSessionTitle, got something else: {:?}",
                std::mem::discriminant(&other)
            ),
            Err(e) => panic!("expected SetSessionTitle, got {e:?}"),
        }
    }

    #[test]
    fn execute_postcard_args_round_trip() {
        // Verify the args can be serialised/deserialised via postcard,
        // which is the wire format used by the VM execution path.
        let args = SetSessionTitleArgs {
            title: "VM Test".into(),
        };
        let args_bytes = postcard::to_allocvec(&args).unwrap();
        let decoded: SetSessionTitleArgs = postcard::from_bytes(&args_bytes).unwrap();
        assert_eq!(decoded.title, "VM Test");
    }
}
