use crate::error::ClientError;
use tai_proto::{DaemonMessage, ImageMetadata, OutputStream, SessionMessage};
use tracing::debug;

pub trait DaemonMessageHandler {
    fn push_text(&mut self, text: String);
    fn push_tool_text(&mut self, _request_id: u32, text: String) {
        self.push_text(text);
    }
    fn push_session_message(&mut self, message: SessionMessage);

    /// Like `push_session_message` but inserts the message *before* the active
    /// stream for `request_id` so that tool results appear before the model's
    /// answer text that follows them.
    fn insert_session_message_before_stream(&mut self, request_id: u32, message: SessionMessage);

    fn begin_stream(&mut self, request_id: u32);
    fn append_stream(&mut self, request_id: u32, stream: OutputStream, chunk: &str);
    fn finalize_stream(&mut self, request_id: u32);
    fn drop_request(&mut self, request_id: u32) {
        self.finalize_stream(request_id);
    }
    fn handle_image_start(
        &mut self,
        request_id: u32,
        metadata: ImageMetadata,
    ) -> Result<(), ClientError>;
    fn handle_image_chunk(
        &mut self,
        request_id: u32,
        image_id: u32,
        data: &[u8],
    ) -> Result<(), ClientError>;
    fn handle_image_end(&mut self, request_id: u32, image_id: u32) -> Result<(), ClientError>;
}

// ── Session lifecycle ─────────────────────────────────────────

fn dispatch_session<H: DaemonMessageHandler>(
    handler: &mut H,
    msg: DaemonMessage,
) -> Result<(), ClientError> {
    match msg {
        DaemonMessage::SessionCreated {
            session_id, title, ..
        } => {
            let label = title.unwrap_or_else(|| "untitled".to_string());
            handler.push_text(format!("[daemon] created session {session_id}: {label}"));
            Ok(())
        }
        DaemonMessage::Sessions { sessions } => {
            if sessions.is_empty() {
                handler.push_text("[daemon] no sessions".to_string());
            } else {
                handler.push_text(format!("[daemon] sessions ({})", sessions.len()));
                for session in &sessions {
                    let title = session.title.as_deref().unwrap_or("untitled");
                    let model = session.selected_model.as_deref().unwrap_or("-");
                    handler.push_text(format!(
                        "  {}: \"{title}\" ({model}) — {} messages",
                        session.session_id, session.message_count
                    ));
                }
            }
            Ok(())
        }
        DaemonMessage::SessionAttached { session_id } => {
            handler.push_text(format!("[daemon] attached session: {session_id}"));
            Ok(())
        }
        DaemonMessage::SessionStatusChanged { .. } => Ok(()),
        DaemonMessage::SessionState {
            session_id,
            title,
            selected_model,
            parent_session_id,
            cwd,
            max_turns,
            active_tool_groups: _,
            token_usage: _,
            messages,
        } => {
            let title = title.unwrap_or_else(|| "untitled".to_string());
            handler.push_text(format!("[daemon] session {session_id}: {title}"));
            if let Some(model) = &selected_model {
                handler.push_text(format!("[daemon]   model: {model}"));
            }
            if let Some(parent) = parent_session_id {
                handler.push_text(format!("[daemon]   parent: {parent}"));
            }
            if let Some(cwd) = &cwd {
                handler.push_text(format!("[daemon]   cwd: {cwd}"));
            }
            if let Some(mt) = max_turns {
                handler.push_text(format!("[daemon]   max-turns: {mt}"));
            }
            handler.push_text(format!("[daemon]   {} messages", messages.len()));
            for message in messages {
                if !matches!(message, SessionMessage::SystemText { .. }) {
                    handler.push_session_message(message);
                }
            }
            Ok(())
        }
        DaemonMessage::SessionFailed { operation, error } => {
            handler.push_text(format!("[daemon] {operation} failed: {error}"));
            Ok(())
        }
        DaemonMessage::SessionMessageAppended { message } => {
            handler.push_session_message(message);
            Ok(())
        }
        DaemonMessage::SessionDeleted { .. } => {
            // Handled upstream by the TUI before dispatch; this crate-level
            // handler just acknowledges it so the match is exhaustive.
            Ok(())
        }
        DaemonMessage::SessionDeleteFailed { .. } => {
            // Handled upstream by the TUI before dispatch; this crate-level
            // handler just acknowledges it so the match is exhaustive.
            Ok(())
        }
        _ => Ok(()),
    }
}

// ── Stream lifecycle (start/end/tool calls) ────────────────────

fn dispatch_stream_lifecycle<H: DaemonMessageHandler>(
    handler: &mut H,
    msg: DaemonMessage,
) -> Result<(), ClientError> {
    match msg {
        DaemonMessage::Started { request_id } => {
            handler.begin_stream(request_id);
            Ok(())
        }
        DaemonMessage::ToolCallStarted {
            request_id,
            call_id,
            tool_name,
            arguments_json,
        } => {
            handler.push_tool_text(
                request_id,
                format!("[{request_id}] tool {tool_name}#{call_id} start {arguments_json}"),
            );
            Ok(())
        }
        DaemonMessage::ToolCallFinished {
            request_id,
            call_id,
            tool_name,
            output,
        } => {
            handler.insert_session_message_before_stream(
                request_id,
                SessionMessage::ToolResult {
                    call_id,
                    name: tool_name,
                    content: output,
                    is_error: false,
                },
            );
            Ok(())
        }
        DaemonMessage::ToolCallFailed {
            request_id,
            call_id,
            tool_name,
            error,
        } => {
            handler.push_tool_text(
                request_id,
                format!("[{request_id}] tool {tool_name}#{call_id} failed: {error}"),
            );
            Ok(())
        }
        DaemonMessage::ToolCallOutput {
            request_id,
            call_id,
            data,
        } => {
            let text = String::from_utf8(data)?;
            handler.push_tool_text(
                request_id,
                format!("[{request_id}] tool #{call_id} output: {text}"),
            );
            Ok(())
        }
        DaemonMessage::Done {
            request_id,
            token_usage: _,
        } => {
            handler.push_text(format!("[{request_id}] done"));
            handler.drop_request(request_id);
            Ok(())
        }
        DaemonMessage::Failed { request_id, error } => {
            handler.push_text(format!("[{request_id}] failed: {error}"));
            handler.drop_request(request_id);
            Ok(())
        }
        DaemonMessage::Cancelled { request_id } => {
            handler.push_text(format!("[{request_id}] cancelled"));
            handler.drop_request(request_id);
            Ok(())
        }
        _ => Ok(()),
    }
}

// ── Image assembly ────────────────────────────────────────────

fn dispatch_image<H: DaemonMessageHandler>(
    handler: &mut H,
    msg: DaemonMessage,
) -> Result<(), ClientError> {
    match msg {
        DaemonMessage::ImageStart {
            request_id,
            metadata,
        } => {
            handler.handle_image_start(request_id, metadata)?;
            Ok(())
        }
        DaemonMessage::ImageChunk {
            request_id,
            image_id,
            data,
        } => {
            handler.handle_image_chunk(request_id, image_id, &data)?;
            Ok(())
        }
        DaemonMessage::ImageEnd {
            request_id,
            image_id,
        } => {
            handler.handle_image_end(request_id, image_id)?;
            Ok(())
        }
        _ => Ok(()),
    }
}

// ── Model management ──────────────────────────────────────────

fn dispatch_model<H: DaemonMessageHandler>(
    handler: &mut H,
    msg: DaemonMessage,
) -> Result<(), ClientError> {
    match msg {
        DaemonMessage::Models {
            models,
            selected_model,
        } => {
            if models.is_empty() {
                handler.push_text("[daemon] no models available".to_string());
            } else {
                handler.push_text(format!("[daemon] supported models ({})", models.len()));
                for model in models {
                    let prefix = if selected_model.as_deref() == Some(model.as_str()) {
                        "*"
                    } else {
                        "-"
                    };
                    handler.push_text(format!("{prefix} {model}"));
                }
            }
            Ok(())
        }
        DaemonMessage::ModelsFailed { error } => {
            handler.push_text(format!("[daemon] models failed: {error}"));
            Ok(())
        }
        DaemonMessage::ModelSelected { model } => {
            handler.push_text(format!("[daemon] selected model: {model}"));
            Ok(())
        }
        DaemonMessage::ModelSelectionFailed { model, error } => {
            handler.push_text(format!("[daemon] failed to select model {model}: {error}"));
            Ok(())
        }
        _ => Ok(()),
    }
}

// ── Keystore state ────────────────────────────────────────────

fn dispatch_keystore<H: DaemonMessageHandler>(
    handler: &mut H,
    msg: DaemonMessage,
) -> Result<(), ClientError> {
    match msg {
        DaemonMessage::Unlocked => {
            handler.push_text("[daemon] keystore unlocked, credentials available".to_string());
            Ok(())
        }
        DaemonMessage::Locked => {
            handler.push_text("[daemon] keystore locked, credentials cleared".to_string());
            Ok(())
        }
        DaemonMessage::LockedError { error } => {
            handler.push_text(format!("[daemon] locked: {error}"));
            Ok(())
        }
        _ => Ok(()),
    }
}

// ── Credential CRUD ───────────────────────────────────────────

fn dispatch_credential<H: DaemonMessageHandler>(
    handler: &mut H,
    msg: DaemonMessage,
) -> Result<(), ClientError> {
    match msg {
        DaemonMessage::CredentialAdded { service } => {
            handler.push_text(format!("[daemon] credential added: {service}"));
            Ok(())
        }
        DaemonMessage::CredentialAddFailed { service, error } => {
            handler.push_text(format!(
                "[daemon] credential add failed ({service}): {error}"
            ));
            Ok(())
        }
        DaemonMessage::CredentialRemoved { service } => {
            handler.push_text(format!("[daemon] credential removed: {service}"));
            Ok(())
        }
        DaemonMessage::CredentialRemoveFailed { service, error } => {
            handler.push_text(format!(
                "[daemon] credential remove failed ({service}): {error}"
            ));
            Ok(())
        }
        DaemonMessage::Credential { .. } => Ok(()),
        _ => Ok(()),
    }
}

// ── Account management ────────────────────────────────────────

fn dispatch_account<H: DaemonMessageHandler>(
    handler: &mut H,
    msg: DaemonMessage,
) -> Result<(), ClientError> {
    match msg {
        DaemonMessage::AccountAdded { name } => {
            handler.push_text(format!("[daemon] account added: {name}"));
            Ok(())
        }
        DaemonMessage::AccountAddFailed { name, error } => {
            handler.push_text(format!("[daemon] failed to add account {name}: {error}"));
            Ok(())
        }
        DaemonMessage::AccountRemoved { name } => {
            handler.push_text(format!("[daemon] account removed: {name}"));
            Ok(())
        }
        DaemonMessage::AccountRemoveFailed { name, error } => {
            handler.push_text(format!("[daemon] failed to remove account {name}: {error}"));
            Ok(())
        }
        DaemonMessage::Accounts { accounts } => {
            if accounts.is_empty() {
                handler.push_text("[daemon] no accounts configured".to_string());
            } else {
                handler.push_text(format!("[daemon] accounts ({})", accounts.len()));
                for a in &accounts {
                    handler.push_text(format!("  {}: {}", a.name, a.provider));
                }
            }
            Ok(())
        }
        DaemonMessage::AccountListFailed { error } => {
            handler.push_text(format!("[daemon] failed to list accounts: {error}"));
            Ok(())
        }
        DaemonMessage::SessionAccountSet { account } => {
            handler.push_text(format!("[daemon] session account set: {account}"));
            Ok(())
        }
        _ => Ok(()),
    }
}

// ── Reasoning effort ──────────────────────────────────────────

fn dispatch_reasoning<H: DaemonMessageHandler>(
    handler: &mut H,
    msg: DaemonMessage,
) -> Result<(), ClientError> {
    match msg {
        DaemonMessage::ReasoningEffortSet { effort } => {
            handler.push_text(format!("[daemon] reasoning effort: {}", effort.as_label()));
            Ok(())
        }
        DaemonMessage::ReasoningEffortSetFailed { effort, error } => {
            handler.push_text(format!(
                "[daemon] failed to set reasoning effort {effort}: {error}",
            ));
            Ok(())
        }
        _ => Ok(()),
    }
}

// ── Miscellaneous one-off messages ────────────────────────────

fn dispatch_misc<H: DaemonMessageHandler>(
    handler: &mut H,
    msg: DaemonMessage,
) -> Result<(), ClientError> {
    match msg {
        DaemonMessage::ShuttingDown => {
            handler.push_text("[daemon] shutting down".to_string());
            Ok(())
        }
        DaemonMessage::Pong => {
            handler.push_text("[daemon] pong".to_string());
            Ok(())
        }
        _ => Ok(()),
    }
}

// ── Main dispatcher ───────────────────────────────────────────

pub fn dispatch_daemon_message<H: DaemonMessageHandler>(
    handler: &mut H,
    message: DaemonMessage,
) -> Result<(), ClientError> {
    debug!("dispatching daemon message: {message:?}");
    match message {
        // Session lifecycle
        m @ (DaemonMessage::SessionCreated { .. }
        | DaemonMessage::Sessions { .. }
        | DaemonMessage::SessionAttached { .. }
        | DaemonMessage::SessionStatusChanged { .. }
        | DaemonMessage::SessionState { .. }
        | DaemonMessage::SessionFailed { .. }
        | DaemonMessage::SessionMessageAppended { .. }
        | DaemonMessage::SessionDeleted { .. }
        | DaemonMessage::SessionDeleteFailed { .. }) => dispatch_session(handler, m),

        // Stream lifecycle (start, finish, tool calls)
        m @ (DaemonMessage::Started { .. }
        | DaemonMessage::ToolCallStarted { .. }
        | DaemonMessage::ToolCallFinished { .. }
        | DaemonMessage::ToolCallFailed { .. }
        | DaemonMessage::ToolCallOutput { .. }
        | DaemonMessage::Done { .. }
        | DaemonMessage::Failed { .. }
        | DaemonMessage::Cancelled { .. }) => dispatch_stream_lifecycle(handler, m),

        // Output chunks (the only variant with its own data flow)
        DaemonMessage::OutputChunk {
            request_id,
            stream,
            data,
        } => {
            let text = String::from_utf8(data)?;
            handler.append_stream(request_id, stream, &text);
            Ok(())
        }

        // Image assembly
        m @ (DaemonMessage::ImageStart { .. }
        | DaemonMessage::ImageChunk { .. }
        | DaemonMessage::ImageEnd { .. }) => dispatch_image(handler, m),

        // Model management
        m @ (DaemonMessage::Models { .. }
        | DaemonMessage::ModelsFailed { .. }
        | DaemonMessage::ModelSelected { .. }
        | DaemonMessage::ModelSelectionFailed { .. }) => dispatch_model(handler, m),

        // Keystore state
        m @ (DaemonMessage::Unlocked
        | DaemonMessage::Locked
        | DaemonMessage::LockedError { .. }) => dispatch_keystore(handler, m),

        // Credential CRUD
        m @ (DaemonMessage::CredentialAdded { .. }
        | DaemonMessage::CredentialAddFailed { .. }
        | DaemonMessage::CredentialRemoved { .. }
        | DaemonMessage::CredentialRemoveFailed { .. }
        | DaemonMessage::Credential { .. }) => dispatch_credential(handler, m),

        // Account management
        m @ (DaemonMessage::AccountAdded { .. }
        | DaemonMessage::AccountAddFailed { .. }
        | DaemonMessage::AccountRemoved { .. }
        | DaemonMessage::AccountRemoveFailed { .. }
        | DaemonMessage::Accounts { .. }
        | DaemonMessage::AccountListFailed { .. }
        | DaemonMessage::SessionAccountSet { .. }) => dispatch_account(handler, m),

        // Reasoning effort
        m @ (DaemonMessage::ReasoningEffortSet { .. }
        | DaemonMessage::ReasoningEffortSetFailed { .. }) => dispatch_reasoning(handler, m),

        // One-off messages
        m @ (DaemonMessage::Pong | DaemonMessage::ShuttingDown) => dispatch_misc(handler, m),

        _ => {
            debug!("unhandled daemon message variant");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tai_proto::{
        AccountInfo, ImageMetadata, OutputStream, SessionMessage, SessionStatus, SessionSummary,
    };

    /// A test handler that records every method call in a VecDeque of events.
    struct TestHandler {
        events: VecDeque<TestEvent>,
        /// Returned by `handle_image_start`, controlled for error-testing.
        image_start_result: Result<(), ClientError>,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum TestEvent {
        PushText(String),
        PushSessionMessage(SessionMessage),
        InsertSessionMessageBeforeStream(u32, SessionMessage),
        BeginStream(u32),
        AppendStream(u32, OutputStream, String),
        FinalizeStream(u32),
        HandleImageStart(u32, ImageMetadata),
        HandleImageChunk(u32, u32, Vec<u8>),
        HandleImageEnd(u32, u32),
    }

    impl TestHandler {
        fn new() -> Self {
            Self {
                events: VecDeque::new(),
                image_start_result: Ok(()),
            }
        }

        fn collect_events(&mut self) -> Vec<TestEvent> {
            self.events.drain(..).collect()
        }
    }

    impl DaemonMessageHandler for TestHandler {
        fn push_text(&mut self, text: String) {
            self.events.push_back(TestEvent::PushText(text));
        }
        fn push_session_message(&mut self, message: SessionMessage) {
            self.events
                .push_back(TestEvent::PushSessionMessage(message));
        }
        fn insert_session_message_before_stream(
            &mut self,
            request_id: u32,
            message: SessionMessage,
        ) {
            self.events
                .push_back(TestEvent::InsertSessionMessageBeforeStream(
                    request_id, message,
                ));
        }
        fn begin_stream(&mut self, request_id: u32) {
            self.events.push_back(TestEvent::BeginStream(request_id));
        }
        fn append_stream(&mut self, request_id: u32, stream: OutputStream, chunk: &str) {
            self.events.push_back(TestEvent::AppendStream(
                request_id,
                stream,
                chunk.to_string(),
            ));
        }
        fn finalize_stream(&mut self, request_id: u32) {
            self.events.push_back(TestEvent::FinalizeStream(request_id));
        }
        fn handle_image_start(
            &mut self,
            request_id: u32,
            metadata: ImageMetadata,
        ) -> Result<(), ClientError> {
            self.events
                .push_back(TestEvent::HandleImageStart(request_id, metadata));
            // Take the stored result and reset to Ok for subsequent calls,
            // so the error is only returned once.
            std::mem::replace(&mut self.image_start_result, Ok(()))
        }
        fn handle_image_chunk(
            &mut self,
            request_id: u32,
            image_id: u32,
            data: &[u8],
        ) -> Result<(), ClientError> {
            self.events.push_back(TestEvent::HandleImageChunk(
                request_id,
                image_id,
                data.to_vec(),
            ));
            Ok(())
        }
        fn handle_image_end(&mut self, request_id: u32, image_id: u32) -> Result<(), ClientError> {
            self.events
                .push_back(TestEvent::HandleImageEnd(request_id, image_id));
            Ok(())
        }
    }

    // ── Helper factories ────────────────────────────────────────────────

    fn sample_image_metadata() -> ImageMetadata {
        ImageMetadata {
            image_id: 1,
            mime_type: "image/png".into(),
            width: 100,
            height: 50,
            byte_len: 4096,
            alt: Some("sample".into()),
        }
    }

    // ── SessionState: SystemText filtering ───────────────────────────────

    #[test]
    fn session_state_filters_system_text() {
        let mut h = TestHandler::new();
        let messages = vec![
            SessionMessage::SystemText {
                content: "system prompt".into(),
            },
            SessionMessage::UserText {
                content: "hello".into(),
            },
        ];
        let msg = DaemonMessage::SessionState {
            session_id: 1,
            title: Some("test".into()),
            selected_model: None,
            parent_session_id: None,
            cwd: None,
            max_turns: None,
            messages,
            active_tool_groups: vec![],
            token_usage: None,
        };
        dispatch_daemon_message(&mut h, msg).unwrap();

        let events = h.collect_events();
        // Should contain intro text lines plus exactly one push_session_message (UserText).
        let pushed: Vec<&TestEvent> = events
            .iter()
            .filter(|e| matches!(e, TestEvent::PushSessionMessage(_)))
            .collect();
        assert_eq!(pushed.len(), 1, "SystemText must be filtered out");
        if let TestEvent::PushSessionMessage(SessionMessage::UserText { content }) = &pushed[0] {
            assert_eq!(content, "hello");
        } else {
            panic!("expected UserText, got {:#?}", pushed[0]);
        }
    }

    #[test]
    fn session_state_passes_non_system_messages() {
        let mut h = TestHandler::new();
        let messages = vec![
            SessionMessage::UserText {
                content: "user msg".into(),
            },
            SessionMessage::AssistantText {
                content: "assistant msg".into(),
                reasoning: None,
                token_usage: None,
            },
            SessionMessage::ToolResult {
                call_id: "c1".into(),
                name: "ls".into(),
                content: "file.txt".into(),
                is_error: false,
            },
        ];
        let msg = DaemonMessage::SessionState {
            session_id: 1,
            title: None,
            selected_model: None,
            parent_session_id: None,
            cwd: None,
            max_turns: None,
            messages,
            active_tool_groups: vec![],
            token_usage: None,
        };
        dispatch_daemon_message(&mut h, msg).unwrap();

        let events = h.collect_events();
        let pushed: Vec<&TestEvent> = events
            .iter()
            .filter(|e| matches!(e, TestEvent::PushSessionMessage(_)))
            .collect();
        assert_eq!(
            pushed.len(),
            3,
            "all non-SystemText messages must pass through"
        );
    }

    #[test]
    fn session_state_empty_messages_is_ok() {
        let mut h = TestHandler::new();
        let msg = DaemonMessage::SessionState {
            session_id: 1,
            title: None,
            selected_model: None,
            parent_session_id: None,
            cwd: None,
            max_turns: None,
            messages: vec![],
            active_tool_groups: vec![],
            token_usage: None,
        };
        dispatch_daemon_message(&mut h, msg).unwrap();
        let events = h.collect_events();
        let pushed: Vec<&TestEvent> = events
            .iter()
            .filter(|e| matches!(e, TestEvent::PushSessionMessage(_)))
            .collect();
        assert!(pushed.is_empty(), "no messages → no pushes");
    }

    #[test]
    fn session_state_show_info_lines() {
        let mut h = TestHandler::new();
        let msg = DaemonMessage::SessionState {
            session_id: 42,
            title: Some("work".into()),
            selected_model: Some("gpt-4".into()),
            parent_session_id: Some(7),
            cwd: Some("/home".into()),
            max_turns: Some(10),
            messages: vec![],
            active_tool_groups: vec![],
            token_usage: None,
        };
        dispatch_daemon_message(&mut h, msg).unwrap();
        let events = h.collect_events();
        // Expect: session title, model, parent, cwd, max-turns, count
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("session 42")))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("gpt-4")))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("7")))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("/home")))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("10")))
        );
    }

    // ── SessionCreated ───────────────────────────────────────────────────

    #[test]
    fn session_created_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::SessionCreated {
                session_id: 5,
                title: Some("new-session".into()),
                parent_session_id: None,
                cwd: None,
                max_turns: None,
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("created session 5")))
        );
    }

    // ── Sessions ─────────────────────────────────────────────────────────

    #[test]
    fn sessions_empty_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(&mut h, DaemonMessage::Sessions { sessions: vec![] }).unwrap();
        let events = h.collect_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("no sessions")))
        );
    }

    #[test]
    fn sessions_non_empty_lists_them() {
        let mut h = TestHandler::new();
        let sessions = vec![SessionSummary {
            session_id: 1,
            title: Some("first".into()),
            selected_model: Some("gpt-4".into()),
            reasoning_effort: None,
            parent_session_id: None,
            cwd: None,
            created_at: 0,
            message_count: 5,
            max_turns: None,
            status: SessionStatus::Inactive,
            active_tool_groups: vec![],
            account_name: None,
            token_usage: None,
        }];
        dispatch_daemon_message(&mut h, DaemonMessage::Sessions { sessions }).unwrap();
        let events = h.collect_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("sessions (1)")))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("first")))
        );
    }

    // ── SessionAttached ──────────────────────────────────────────────────

    #[test]
    fn session_attached_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(&mut h, DaemonMessage::SessionAttached { session_id: 3 }).unwrap();
        let events = h.collect_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("attached session: 3")))
        );
    }

    // ── SessionStatusChanged ─────────────────────────────────────────────

    #[test]
    fn session_status_changed_is_noop() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::SessionStatusChanged {
                session_id: 1,
                status: SessionStatus::Inference,
            },
        )
        .unwrap();
        assert!(h.collect_events().is_empty());
    }

    // ── SessionFailed ────────────────────────────────────────────────────

    #[test]
    fn session_failed_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::SessionFailed {
                operation: "create".into(),
                error: "timeout".into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(
            events.iter().any(
                |e| matches!(e, TestEvent::PushText(t) if t.contains("create failed: timeout"))
            )
        );
    }

    // ── SessionMessageAppended ───────────────────────────────────────────

    #[test]
    fn session_message_appended_pushes_message() {
        let mut h = TestHandler::new();
        let msg = SessionMessage::UserText {
            content: "hi".into(),
        };
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::SessionMessageAppended {
                message: msg.clone(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushSessionMessage(m) if *m == msg))
        );
    }

    // ── Started / Done / Failed / Cancelled ──────────────────────────────

    #[test]
    fn started_begins_stream() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(&mut h, DaemonMessage::Started { request_id: 7 }).unwrap();
        let events = h.collect_events();
        assert!(events.contains(&TestEvent::BeginStream(7)));
    }

    #[test]
    fn done_pushes_text_and_drops_request() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::Done {
                request_id: 7,
                token_usage: None,
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("done")))
        );
        assert!(events.contains(&TestEvent::FinalizeStream(7)));
    }

    #[test]
    fn failed_pushes_text_and_drops_request() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::Failed {
                request_id: 7,
                error: "oops".into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("[7] failed: oops")))
        );
        assert!(events.contains(&TestEvent::FinalizeStream(7)));
    }

    #[test]
    fn cancelled_pushes_text_and_drops_request() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(&mut h, DaemonMessage::Cancelled { request_id: 7 }).unwrap();
        let events = h.collect_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("[7] cancelled")))
        );
        assert!(events.contains(&TestEvent::FinalizeStream(7)));
    }

    // ── OutputChunk ──────────────────────────────────────────────────────

    #[test]
    fn output_chunk_appends_stream() {
        let mut h = TestHandler::new();
        let data = "hello world".to_string().into_bytes();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::OutputChunk {
                request_id: 7,
                stream: OutputStream::Answer,
                data,
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(events.contains(&TestEvent::AppendStream(
            7,
            OutputStream::Answer,
            "hello world".into()
        )));
    }

    // ── Tool calls ───────────────────────────────────────────────────────

    #[test]
    fn tool_call_started_pushes_tool_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::ToolCallStarted {
                request_id: 7,
                call_id: "call_1".into(),
                tool_name: "read_file".into(),
                arguments_json: r#"{"path": "/tmp/x"}"#.into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(events.iter().any(
            |e| matches!(e, TestEvent::PushText(t) if t.contains("tool read_file#call_1 start"))
        ));
    }

    #[test]
    fn tool_call_finished_inserts_tool_result() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::ToolCallFinished {
                request_id: 7,
                call_id: "c1".into(),
                tool_name: "ls".into(),
                output: "file.txt".into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        let expected = TestEvent::InsertSessionMessageBeforeStream(
            7,
            SessionMessage::ToolResult {
                call_id: "c1".into(),
                name: "ls".into(),
                content: "file.txt".into(),
                is_error: false,
            },
        );
        assert!(events.contains(&expected));
    }

    #[test]
    fn tool_call_failed_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::ToolCallFailed {
                request_id: 7,
                call_id: "c1".into(),
                tool_name: "ls".into(),
                error: "permission denied".into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("tool ls#c1 failed")))
        );
    }

    #[test]
    fn tool_call_output_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::ToolCallOutput {
                request_id: 7,
                call_id: "c1".into(),
                data: b"intermediate output".to_vec(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(events.iter().any(|e| matches!(e, TestEvent::PushText(t) if t.contains("tool #c1 output: intermediate output"))));
    }

    #[test]
    fn tool_call_output_invalid_utf8_returns_error() {
        let mut h = TestHandler::new();
        let result = dispatch_daemon_message(
            &mut h,
            DaemonMessage::ToolCallOutput {
                request_id: 7,
                call_id: "c1".into(),
                data: vec![0xFF, 0xFE], // invalid UTF-8
            },
        );
        assert!(result.is_err(), "invalid UTF-8 must produce an error");
    }

    // ── Image handling ───────────────────────────────────────────────────

    #[test]
    fn image_start_forwards_to_handler() {
        let mut h = TestHandler::new();
        let metadata = sample_image_metadata();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::ImageStart {
                request_id: 7,
                metadata: metadata.clone(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(events.contains(&TestEvent::HandleImageStart(7, metadata)));
    }

    #[test]
    fn image_start_error_propagates() {
        let mut h = TestHandler::new();
        h.image_start_result = Err(ClientError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            "no space",
        )));
        let result = dispatch_daemon_message(
            &mut h,
            DaemonMessage::ImageStart {
                request_id: 7,
                metadata: sample_image_metadata(),
            },
        );
        assert!(result.is_err(), "handler error must propagate");
    }

    #[test]
    fn image_chunk_forwards_to_handler() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::ImageChunk {
                request_id: 7,
                image_id: 1,
                data: b"chunk data".to_vec(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(events.contains(&TestEvent::HandleImageChunk(7, 1, b"chunk data".to_vec())));
    }

    #[test]
    fn image_end_forwards_to_handler() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::ImageEnd {
                request_id: 7,
                image_id: 1,
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(events.contains(&TestEvent::HandleImageEnd(7, 1)));
    }

    // ── Pong ─────────────────────────────────────────────────────────────

    #[test]
    fn pong_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(&mut h, DaemonMessage::Pong).unwrap();
        let events = h.collect_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("pong")))
        );
    }

    // ── Models ───────────────────────────────────────────────────────────

    #[test]
    fn models_empty_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::Models {
                models: vec![],
                selected_model: None,
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("no models available")))
        );
    }

    #[test]
    fn models_non_empty_lists_them() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::Models {
                models: vec!["gpt-4".into(), "gpt-3.5".into()],
                selected_model: Some("gpt-4".into()),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("supported models (2)")))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("* gpt-4")))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("- gpt-3.5")))
        );
    }

    // ── ModelsFailed / ModelSelected / ModelSelectionFailed ──────────────

    #[test]
    fn models_failed_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::ModelsFailed {
                error: "rate limited".into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(events.iter().any(
            |e| matches!(e, TestEvent::PushText(t) if t.contains("models failed: rate limited"))
        ));
    }

    #[test]
    fn model_selected_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::ModelSelected {
                model: "gpt-4".into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(
            events.iter().any(
                |e| matches!(e, TestEvent::PushText(t) if t.contains("selected model: gpt-4"))
            )
        );
    }

    #[test]
    fn model_selection_failed_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::ModelSelectionFailed {
                model: "gpt-4".into(),
                error: "not found".into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(
            events.iter().any(|e| matches!(e, TestEvent::PushText(t) if t.contains("failed to select model gpt-4: not found")))
        );
    }

    // ── Keystore (Unlocked / Locked / LockedError) ───────────────────────

    #[test]
    fn unlocked_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(&mut h, DaemonMessage::Unlocked).unwrap();
        let events = h.collect_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("keystore unlocked")))
        );
    }

    #[test]
    fn locked_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(&mut h, DaemonMessage::Locked).unwrap();
        let events = h.collect_events();
        assert!(events.iter().any(|e| matches!(e, TestEvent::PushText(t) if t.contains("keystore locked, credentials cleared"))));
    }

    #[test]
    fn locked_error_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::LockedError {
                error: "wrong passphrase".into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(events.iter().any(
            |e| matches!(e, TestEvent::PushText(t) if t.contains("locked: wrong passphrase"))
        ));
    }

    // ── Credentials ──────────────────────────────────────────────────────

    #[test]
    fn credential_added_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::CredentialAdded {
                service: "my-account".into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(events.iter().any(
            |e| matches!(e, TestEvent::PushText(t) if t.contains("credential added: my-account"))
        ));
    }

    #[test]
    fn credential_add_failed_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::CredentialAddFailed {
                service: "my-account".into(),
                error: "bad key".into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(
            events.iter().any(|e| matches!(e, TestEvent::PushText(t) if t.contains("credential add failed (my-account): bad key")))
        );
    }

    #[test]
    fn credential_removed_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::CredentialRemoved {
                service: "my-account".into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(events.iter().any(
            |e| matches!(e, TestEvent::PushText(t) if t.contains("credential removed: my-account"))
        ));
    }

    #[test]
    fn credential_remove_failed_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::CredentialRemoveFailed {
                service: "my-account".into(),
                error: "not found".into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(
            events.iter().any(|e| matches!(e, TestEvent::PushText(t) if t.contains("credential remove failed (my-account): not found")))
        );
    }

    #[test]
    fn credential_variant_is_noop() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::Credential {
                service: "x".into(),
                key: None,
            },
        )
        .unwrap();
        assert!(h.collect_events().is_empty());
    }

    // ── SessionDeleted / SessionDeleteFailed ────────────────────────────

    #[test]
    fn session_deleted_is_noop() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(&mut h, DaemonMessage::SessionDeleted { session_id: 1 }).unwrap();
        assert!(h.collect_events().is_empty());
    }

    #[test]
    fn session_delete_failed_is_noop() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::SessionDeleteFailed {
                session_id: 1,
                error: "locked".into(),
            },
        )
        .unwrap();
        assert!(h.collect_events().is_empty());
    }

    // ── ShuttingDown ─────────────────────────────────────────────────────

    #[test]
    fn shutting_down_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(&mut h, DaemonMessage::ShuttingDown).unwrap();
        let events = h.collect_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("shutting down")))
        );
    }

    // ── AI Provider Accounts ─────────────────────────────────────────────

    #[test]
    fn account_added_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::AccountAdded {
                name: "my-provider".into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(events.iter().any(
            |e| matches!(e, TestEvent::PushText(t) if t.contains("account added: my-provider"))
        ));
    }

    #[test]
    fn account_add_failed_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::AccountAddFailed {
                name: "my-provider".into(),
                error: "exists".into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(
            events.iter().any(|e| matches!(e, TestEvent::PushText(t) if t.contains("failed to add account my-provider: exists")))
        );
    }

    #[test]
    fn account_removed_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::AccountRemoved {
                name: "my-provider".into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(events.iter().any(
            |e| matches!(e, TestEvent::PushText(t) if t.contains("account removed: my-provider"))
        ));
    }

    #[test]
    fn account_remove_failed_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::AccountRemoveFailed {
                name: "my-provider".into(),
                error: "not found".into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(
            events.iter().any(|e| matches!(e, TestEvent::PushText(t) if t.contains("failed to remove account my-provider: not found")))
        );
    }

    #[test]
    fn accounts_empty_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(&mut h, DaemonMessage::Accounts { accounts: vec![] }).unwrap();
        let events = h.collect_events();
        assert!(
            events.iter().any(
                |e| matches!(e, TestEvent::PushText(t) if t.contains("no accounts configured"))
            )
        );
    }

    #[test]
    fn accounts_non_empty_lists_them() {
        let mut h = TestHandler::new();
        let accounts = vec![AccountInfo {
            name: "my-acc".into(),
            provider: "opencode".into(),
            has_credential: true,
        }];
        dispatch_daemon_message(&mut h, DaemonMessage::Accounts { accounts }).unwrap();
        let events = h.collect_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("accounts (1)")))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TestEvent::PushText(t) if t.contains("my-acc: opencode")))
        );
    }

    #[test]
    fn account_list_failed_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::AccountListFailed {
                error: "daemon locked".into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(events.iter().any(|e| matches!(e, TestEvent::PushText(t) if t.contains("failed to list accounts: daemon locked"))));
    }

    #[test]
    fn session_account_set_pushes_text() {
        let mut h = TestHandler::new();
        dispatch_daemon_message(
            &mut h,
            DaemonMessage::SessionAccountSet {
                account: "my-acc".into(),
            },
        )
        .unwrap();
        let events = h.collect_events();
        assert!(events.iter().any(
            |e| matches!(e, TestEvent::PushText(t) if t.contains("session account set: my-acc"))
        ));
    }

    // ── Return value tests ───────────────────────────────────────────────

    #[test]
    fn all_variants_succeed() {
        // Spot-check a few variants — they should all return Ok(()).
        let variants: Vec<DaemonMessage> = vec![
            DaemonMessage::Pong,
            DaemonMessage::Unlocked,
            DaemonMessage::Locked,
            DaemonMessage::ShuttingDown,
            DaemonMessage::SessionStatusChanged {
                session_id: 1,
                status: SessionStatus::Inactive,
            },
            DaemonMessage::Credential {
                service: "x".into(),
                key: None,
            },
            DaemonMessage::SessionDeleted { session_id: 1 },
            DaemonMessage::SessionDeleteFailed {
                session_id: 1,
                error: "x".into(),
            },
        ];
        let mut h = TestHandler::new();
        for variant in variants {
            let result = dispatch_daemon_message(&mut h, variant);
            assert!(result.is_ok(), "expected Ok(()), got {result:?}");
            result.unwrap();
        }
    }

    #[test]
    fn output_chunk_invalid_utf8_returns_error() {
        let mut h = TestHandler::new();
        let result = dispatch_daemon_message(
            &mut h,
            DaemonMessage::OutputChunk {
                request_id: 7,
                stream: OutputStream::Answer,
                data: vec![0xFF, 0xFE],
            },
        );
        assert!(result.is_err(), "invalid UTF-8 must produce an error");
    }
}
