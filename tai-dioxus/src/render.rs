use crate::state::{HistoryItem, StreamingEntry};
use dioxus::prelude::*;
use tai_client_core::FileDiff;
use tai_client_core::HistoryItem as SharedHistoryItem;
use tai_markdown::render_markdown_html;
use tai_proto::{ImageMetadata, SessionMessage};

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
                    {files.iter().map(format_diff_file).collect::<Vec<_>>().join("\n")}
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

    fn make_diff(diff_lines: Vec<(DiffLineKind, &str)>) -> FileDiff {
        FileDiff {
            old_path: "test.rs".into(),
            new_path: "test.rs".into(),
            hunks: vec![DiffHunk {
                header: "@@ -1 +1 @@".into(),
                lines: diff_lines
                    .into_iter()
                    .map(|(kind, content)| DiffLine {
                        kind,
                        content: content.into(),
                    })
                    .collect(),
            }],
        }
    }

    #[test]
    fn format_diff_file_includes_headers() {
        let fd = make_diff(vec![]);
        let out = super::format_diff_file(&fd);
        assert!(out.contains("--- test.rs"));
        assert!(out.contains("+++ test.rs"));
    }

    #[test]
    fn format_diff_file_shows_additions() {
        let fd = make_diff(vec![(DiffLineKind::Addition, "new_line")]);
        let out = super::format_diff_file(&fd);
        assert!(out.contains("+new_line"));
    }

    #[test]
    fn format_diff_file_shows_deletions() {
        let fd = make_diff(vec![(DiffLineKind::Deletion, "old_line")]);
        let out = super::format_diff_file(&fd);
        assert!(out.contains("-old_line"));
    }

    #[test]
    fn format_diff_file_shows_context() {
        let fd = make_diff(vec![(DiffLineKind::Context, "ctx")]);
        let out = super::format_diff_file(&fd);
        assert!(out.contains(" ctx"));
    }
}

fn render_system_text(content: &str) -> Element {
    render_labeled_plain_message("system", content, "session-item system-item")
}

fn render_user_text(content: &str) -> Element {
    render_labeled_plain_message("user", content, "session-item user-item")
}

fn render_assistant_text(content: &str, reasoning: &Option<String>) -> Element {
    let html = render_markdown_html(content);
    let reasoning_text = reasoning
        .as_ref()
        .and_then(|r| (!r.trim().is_empty()).then(|| r.clone()));
    rsx! {
        div { class: "history-item session-item assistant-item",
            div { class: "message-label", "assistant" }
            if let Some(text) = reasoning_text {
                div { class: "stream-section reasoning",
                    div { class: "label", "reasoning" }
                    pre { class: "plain-body", "{text}" }
                }
            }
            div { class: "markdown-body", dangerous_inner_html: "{html}" }
        }
    }
}

fn render_tool_use(name: &str, reasoning: &Option<String>, content: &str) -> Element {
    let content_html = (!content.trim().is_empty()).then(|| render_markdown_html(content));
    rsx! {
        div { class: "history-item session-item tool-use-item",
            div { class: "message-label", "tool call" }
            pre { class: "plain-body", "{name}" }
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

fn render_tool_result(is_error: bool, content: &str) -> Element {
    let label = if is_error {
        "tool error"
    } else {
        "tool result"
    };
    render_labeled_plain_message(label, content, "session-item tool-result-item")
}

fn render_displayed_image(metadata: &ImageMetadata) -> Element {
    rsx! {
        div { class: "history-item session-item image-item",
            div { class: "message-label", "image" }
            pre { class: "plain-body",
                "[displayed image: {metadata.mime_type} ({metadata.width}x{metadata.height})]"
            }
        }
    }
}

fn render_session_message(message: SessionMessage) -> Element {
    match message {
        SessionMessage::SystemText { content } => render_system_text(&content),
        SessionMessage::UserText { content } => render_user_text(&content),
        SessionMessage::AssistantText {
            content, reasoning, ..
        } => render_assistant_text(&content, &reasoning),
        SessionMessage::AssistantToolUse {
            content,
            tool_calls,
            reasoning,
            ..
        } => {
            let name = tool_calls
                .iter()
                .map(|call| format!("{}({})", call.name, call.arguments_json))
                .collect::<Vec<_>>()
                .join(", ");
            let resolved_reasoning = reasoning.filter(|value| !value.trim().is_empty());
            let content = content
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_default();
            render_tool_use(&name, &resolved_reasoning, &content)
        }
        SessionMessage::ToolResult {
            name,
            content,
            is_error,
            ..
        } => render_tool_result(is_error, &format!("{name}: {content}")),
        SessionMessage::DisplayedImage(record) => render_displayed_image(&record.metadata),
        _ => rsx! {},
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
