use crate::state::{HistoryItem, StreamingEntry};
use dioxus::prelude::*;
use tai_client_core::HistoryItem as SharedHistoryItem;
use tai_client_core::render_markdown_html;
use tai_client_core::FileDiff;
use tai_proto::SessionMessage;

pub(crate) fn render_history_item(item: HistoryItem) -> Element {
    match item {
        SharedHistoryItem::Text(text) => rsx! {
            div { class: "history-item text-item",
                pre { "{text}" }
            }
        },
        SharedHistoryItem::SessionMessage(message) => render_session_message(message),
        SharedHistoryItem::Streaming(entry) => render_streaming_entry(entry),
        SharedHistoryItem::Image(image) => rsx! {
            div { class: "history-item image-item",
                div { class: "image-meta",
                    {format!(
                        "image {} ({} {}x{})",
                        image.metadata.image_id,
                        image.metadata.mime_type,
                        image.metadata.width,
                        image.metadata.height
                    )}
                }
                img {
                    class: "history-image",
                    src: image.data_url.clone(),
                    alt: image
                        .metadata
                        .alt
                        .clone()
                        .unwrap_or_else(|| String::from("image"))
                }
            }
        },
        SharedHistoryItem::Diff(files) => rsx! {
            div { class: "history-item diff-item",
                div { class: "diff-header", "Diff ({files.len()} file(s))" }
                pre { class: "diff-body",
                    {files.iter().map(|f| format_diff_file(f)).collect::<Vec<_>>().join("\n")}
                }
            }
        },
    }
}

fn format_diff_file(file: &FileDiff) -> String {
    let mut out = format!("--- {}\n+++ {}\n", file.old_path, file.new_path);
    for hunk in &file.hunks {
        out.push_str(&format!("{}\n", hunk.header));
        for line in &hunk.lines {
            let prefix = match line.kind {
                tai_client_core::DiffLineKind::Addition => "+",
                tai_client_core::DiffLineKind::Deletion => "-",
                tai_client_core::DiffLineKind::Context => " ",
            };
            out.push_str(&format!("{}{}\n", prefix, line.content));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use tai_client_core::{DiffHunk, DiffLine, DiffLineKind, FileDiff};

    #[test]
    fn format_diff_file_includes_headers() {
        let fd = FileDiff {
            old_path: "old.rs".into(),
            new_path: "new.rs".into(),
            hunks: vec![],
        };
        let out = super::format_diff_file(&fd);
        assert!(out.contains("--- old.rs"));
        assert!(out.contains("+++ new.rs"));
    }

    #[test]
    fn format_diff_file_shows_additions() {
        let fd = FileDiff {
            old_path: "f".into(),
            new_path: "f".into(),
            hunks: vec![DiffHunk {
                header: "@@ -1 +1 @@".into(),
                lines: vec![
                    DiffLine { kind: DiffLineKind::Addition, content: "new_line".into() },
                ],
            }],
        };
        let out = super::format_diff_file(&fd);
        assert!(out.contains("+new_line"));
    }

    #[test]
    fn format_diff_file_shows_deletions() {
        let fd = FileDiff {
            old_path: "f".into(),
            new_path: "f".into(),
            hunks: vec![DiffHunk {
                header: "@@ -1 +1 @@".into(),
                lines: vec![
                    DiffLine { kind: DiffLineKind::Deletion, content: "old_line".into() },
                ],
            }],
        };
        let out = super::format_diff_file(&fd);
        assert!(out.contains("-old_line"));
    }

    #[test]
    fn format_diff_file_shows_context() {
        let fd = FileDiff {
            old_path: "f".into(),
            new_path: "f".into(),
            hunks: vec![DiffHunk {
                header: "@@ -1 +1 @@".into(),
                lines: vec![
                    DiffLine { kind: DiffLineKind::Context, content: "ctx".into() },
                ],
            }],
        };
        let out = super::format_diff_file(&fd);
        assert!(out.contains(" ctx"));
    }
}

fn render_session_message(message: SessionMessage) -> Element {
    match message {
        SessionMessage::SystemText { content } => {
            render_labeled_plain_message("system", content, "session-item system-item")
        }
        SessionMessage::UserText { content } => {
            render_labeled_plain_message("user", content, "session-item user-item")
        }
        SessionMessage::AssistantText { content } => {
            let html = render_markdown_html(&content);
            rsx! {
                div { class: "history-item session-item assistant-item",
                    div { class: "message-label", "assistant" }
                    div { class: "markdown-body", dangerous_inner_html: "{html}" }
                }
            }
        }
        SessionMessage::AssistantToolUse {
            content,
            tool_calls,
            reasoning_content,
            reasoning,
            reasoning_text,
        } => {
            let tool_call_text = tool_calls
                .iter()
                .map(|call| format!("{}({})", call.name, call.arguments_json))
                .collect::<Vec<_>>()
                .join(", ");
            let reasoning = reasoning_content
                .or(reasoning)
                .or(reasoning_text)
                .filter(|value| !value.trim().is_empty());
            let content_html = content
                .filter(|value| !value.trim().is_empty())
                .map(|value| render_markdown_html(&value));
            rsx! {
                div { class: "history-item session-item tool-use-item",
                    div { class: "message-label", "tool call" }
                    pre { class: "plain-body", "{tool_call_text}" }
                    if let Some(reasoning) = reasoning {
                        div { class: "stream-section reasoning",
                            div { class: "label", "reasoning" }
                            pre { class: "plain-body", "{reasoning}" }
                        }
                    }
                    if let Some(content_html) = content_html {
                        div { class: "stream-section answer markdown-section",
                            div { class: "label", "content" }
                            div { class: "markdown-body", dangerous_inner_html: "{content_html}" }
                        }
                    }
                }
            }
        }
        SessionMessage::ToolResult {
            name,
            content,
            is_error,
            ..
        } => {
            let label = if is_error {
                "tool error"
            } else {
                "tool result"
            };
            render_labeled_plain_message(
                label,
                format!("{name}: {content}"),
                "session-item tool-result-item",
            )
        }
    }
}

fn render_labeled_plain_message(
    label: &'static str,
    content: impl Into<String>,
    class: &'static str,
) -> Element {
    let content = content.into();
    rsx! {
        div { class: "history-item {class}",
            div { class: "message-label", "{label}" }
            pre { class: "plain-body", "{content}" }
        }
    }
}

fn render_streaming_entry(entry: StreamingEntry) -> Element {
    let answer_html =
        (!entry.answer.trim().is_empty()).then(|| render_markdown_html(&entry.answer));
    rsx! {
        div { class: "history-item stream-item",
            div { class: "request-id", "[{entry.request_id}]" }
            if !entry.reasoning.is_empty() {
                div { class: "stream-section reasoning",
                    div { class: "label", "reasoning" }
                    pre { class: "plain-body", "{entry.reasoning}" }
                }
            }
            if let Some(answer_html) = answer_html {
                div { class: "stream-section answer markdown-section",
                    div { class: "label", "answer" }
                    div { class: "markdown-body", dangerous_inner_html: "{answer_html}" }
                }
            }
        }
    }
}
