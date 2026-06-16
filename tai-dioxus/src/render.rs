use crate::state::{HistoryItem, StreamingEntry};
use tai_client_core::HistoryItem as SharedHistoryItem;
use dioxus::prelude::*;
use tai_client_core::render_markdown_html;
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
