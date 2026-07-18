use dioxus::prelude::*;
use tai_markdown::render_markdown_html;
use tai_proto::{AssistantToolCallRecord, DisplayedImageRecord, ToolResultRecord, Turn};

pub(crate) fn render_turn(turn_id: u32, turn: &Turn) -> Element {
    let mut children: Vec<Element> = Vec::new();

    if turn.undone {
        children.push(rsx! {
            div { class: "history-item text-item", pre { "[undone]" } }
        });
    }

    if let Some(ref text) = turn.user_text {
        children.push(render_user_text(text));
    }

    if let Some(ref reasoning) = turn.assistant_reasoning {
        children.push(render_reasoning(reasoning));
    }

    if let Some(ref text) = turn.assistant_text {
        children.push(render_assistant_text(text));
    }

    for tc in &turn.tool_calls {
        children.push(render_tool_call(tc));
    }

    for tr in &turn.tool_results {
        children.push(render_tool_result(tr));
    }

    for img in &turn.displayed_images {
        children.push(render_displayed_image(img));
    }

    if let Some(ref error) = turn.error {
        children.push(render_labeled_plain_message(
            "error",
            error,
            "session-item error-item",
        ));
    }

    rsx! {
        div { class: "turn", key: "{turn_id}",
            div { class: "turn-header", "Turn #{turn_id}" }
            for child in children {
                {child}
            }
        }
    }
}

fn render_reasoning(content: &str) -> Element {
    rsx! {
        div { class: "history-item session-item reasoning-section",
            div { class: "message-label", "reasoning" }
            pre { class: "plain-body", "{content}" }
        }
    }
}

fn render_user_text(content: &str) -> Element {
    render_labeled_plain_message("user", content, "session-item user-item")
}

fn render_assistant_text(content: &str) -> Element {
    let html = render_markdown_html(content);
    rsx! {
        div { class: "history-item session-item assistant-item",
            div { class: "message-label", "assistant" }
            div { class: "markdown-body", dangerous_inner_html: "{html}" }
        }
    }
}

fn render_tool_call(tc: &AssistantToolCallRecord) -> Element {
    let name = humfmt::list(&[format!("{}({})", tc.name, tc.arguments_json)]).to_string();
    render_labeled_plain_message("tool call", &name, "session-item tool-use-item")
}

fn render_tool_result(tr: &ToolResultRecord) -> Element {
    let label = if tr.is_error {
        "tool error"
    } else {
        "tool result"
    };
    let content = format!("{}: {}", tr.name, tr.content);
    render_labeled_plain_message(label, &content, "session-item tool-result-item")
}

fn render_displayed_image(record: &DisplayedImageRecord) -> Element {
    let label = format!(
        "[displayed image: {} ({}x{})]",
        record.metadata.mime_type, record.metadata.width, record.metadata.height,
    );
    rsx! {
        div { class: "history-item session-item image-item",
            div { class: "message-label", "image" }
            pre { class: "plain-body", "{label}" }
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
