use ammonia::Builder as HtmlSanitizer;
use frankenstein::client_ureq::Bot;
use frankenstein::{ParseMode, TelegramApi};
use frankenstein::methods::{GetUpdatesParams, SendMessageParams, SendPhotoParams};
use frankenstein::types::ChatType;
use std::sync::mpsc;
use std::cell::Cell;
use tai_client_core::{ShellCommand, parse_input_line};
use tai_markdown::render_markdown_html;
use tai_proto::ClientMessage;
use tracing::{debug, error, info, warn};

use crate::bridge::BridgeEvent;

pub fn run(
    bot_token: String,
    admin_ids: Vec<i64>,
    bridge_tx: mpsc::Sender<ClientMessage>,
    bridge_rx: mpsc::Receiver<BridgeEvent>,
) {
    let bot = Bot::new(&bot_token);

    let (chat_id_tx, chat_id_rx) = mpsc::channel();

    {
        let bot = bot.clone();
        std::thread::spawn(move || {
            let mut chat_id: Option<i64> = None;
            while let Ok(event) = bridge_rx.recv() {
                debug!(?event, "bridge event received");
                while let Ok(cid) = chat_id_rx.try_recv() {
                    chat_id = Some(cid);
                }
                if let Some(cid) = chat_id {
                    send_daemon_event(&bot, cid, event);
                } else {
                    warn!("no chat id set, dropping bridge event");
                }
            }
            info!("bridge event stream ended");
        });
    }

    let state = TelegramState {
        bridge_tx,
        admin_ids,
        request_id: Cell::new(0),
        chat_id_tx,
    };

    info!("starting telegram bot polling");

    let mut update_id: u32 = 0;
    loop {
        let params = GetUpdatesParams::builder()
            .offset(update_id as i64 + 1)
            .timeout(10)
            .build();
        match bot.get_updates(&params) {
            Ok(response) => {
                for update in response.result {
                    if let frankenstein::updates::UpdateContent::Message(msg) = update.content {
                        update_id = update.update_id;
                        handle_message(&bot, &state, *msg);
                    } else {
                        update_id = update.update_id;
                    }
                }
            }
            Err(e) => {
                error!(%e, "failed to get updates, retrying in 5s");
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        }
    }
}

struct TelegramState {
    bridge_tx: mpsc::Sender<ClientMessage>,
    admin_ids: Vec<i64>,
    request_id: Cell<u32>,
    chat_id_tx: mpsc::Sender<i64>,
}

fn is_chat_private(msg: &frankenstein::types::Message) -> bool {
    msg.chat.type_field == ChatType::Private
}

fn is_admin(msg: &frankenstein::types::Message, admin_ids: &[i64]) -> bool {
    msg.from
        .as_ref()
        .is_some_and(|user| admin_ids.contains(&(user.id as i64)))
}

fn handle_message(bot: &Bot, state: &TelegramState, msg: frankenstein::types::Message) {
    let text = match msg.text.as_ref() {
        Some(t) => t.clone(),
        None => return,
    };

    let user_id = msg.from.as_ref().map(|u| u.id).unwrap_or(0);
    if !(is_chat_private(&msg) && is_admin(&msg, &state.admin_ids)) {
        debug!(%user_id, "non-admin or non-private message ignored");
        return;
    }

    let chat_id_val = msg.chat.id;
    debug!(%user_id, input = %text, "received telegram message");

    let _ = state.chat_id_tx.send(chat_id_val);

    let mut request_id = state.request_id.get();
    let command = parse_input_line(&text, &mut request_id, None);
    state.request_id.set(request_id);

    match command {
        ShellCommand::Send(client_msg) => {
            if let ClientMessage::RunInput { input, .. } = &client_msg {
                let echo = format!("> {}", String::from_utf8_lossy(input));
                let params = SendMessageParams::builder()
                    .chat_id(chat_id_val)
                    .text(echo)
                    .build();
                if let Err(e) = bot.send_message(&params) {
                    warn!("failed to send echo to telegram: {e}");
                }
            }
            debug!("sending command to bridge");
            if let Err(e) = state.bridge_tx.send(client_msg) {
                warn!("failed to send command to bridge: {e}");
            }
        }
        ShellCommand::UnknownCommand(err) => {
            warn!(%err, "unknown command from user");
            let params = SendMessageParams::builder()
                .chat_id(chat_id_val)
                .text(err)
                .build();
            if let Err(e) = bot.send_message(&params) {
                warn!("failed to send error message to telegram: {e}");
            }
        }
        ShellCommand::Empty | ShellCommand::InvalidCancel(_) => {}
    }
}

fn send_daemon_event(bot: &Bot, chat_id: i64, event: BridgeEvent) {
    match event {
        BridgeEvent::Text(text) => {
            let html = render_markdown_html(&text);
            let tg_html = to_telegram_html(&html);
            if !tg_html.is_empty() {
                let params = SendMessageParams::builder()
                    .chat_id(chat_id)
                    .text(tg_html)
                    .parse_mode(ParseMode::Html)
                    .build();
                if let Err(e) = bot.send_message(&params) {
                    error!(%e, "failed to send text message to telegram");
                }
            }
        }
        BridgeEvent::ToolCallStarted {
            name,
            arguments_json,
        } => {
            let params = SendMessageParams::builder()
                .chat_id(chat_id)
                .text(format!("<i>Running {name} {arguments_json}</i>"))
                .parse_mode(ParseMode::Html)
                .build();
            if let Err(e) = bot.send_message(&params) {
                warn!("failed to send tool call started message: {e}");
            }
        }
        BridgeEvent::ToolCallFinished { name, output } => {
            let params = SendMessageParams::builder()
                .chat_id(chat_id)
                .text(format!("<b>{name}</b>: {output}"))
                .parse_mode(ParseMode::Html)
                .build();
            if let Err(e) = bot.send_message(&params) {
                warn!("failed to send tool call finished message: {e}");
            }
        }
        BridgeEvent::ToolCallFailed {
            name,
            error: error_msg,
        } => {
            error!(%name, %error_msg, "tool call failed");
            let params = SendMessageParams::builder()
                .chat_id(chat_id)
                .text(format!("<b>{name}</b> failed: {error_msg}"))
                .parse_mode(ParseMode::Html)
                .build();
            if let Err(e) = bot.send_message(&params) {
                warn!("failed to send tool call failed message: {e}");
            }
        }
        BridgeEvent::Image { data, .. } => {
            let result = send_photo_from_memory(bot, chat_id, &data);
            if let Err(e) = result {
                error!(%e, "failed to send image to telegram");
            }
        }
        BridgeEvent::Error(msg) => {
            error!(%msg, "sending error to telegram");
            let params = SendMessageParams::builder()
                .chat_id(chat_id)
                .text(msg)
                .build();
            if let Err(e) = bot.send_message(&params) {
                warn!("failed to send error event to telegram: {e}");
            }
        }
        BridgeEvent::Models { models, selected } => {
            let mut text = String::from("Models:\n");
            for model in &models {
                let marker = if Some(model.as_str()) == selected.as_deref() {
                    "*"
                } else {
                    "-"
                };
                text.push_str(&format!("  {marker} {model}\n"));
            }
            let params = SendMessageParams::builder()
                .chat_id(chat_id)
                .text(text)
                .build();
            if let Err(e) = bot.send_message(&params) {
                warn!("failed to send models list to telegram: {e}");
            }
        }
        BridgeEvent::ModelSelected(model) => {
            let params = SendMessageParams::builder()
                .chat_id(chat_id)
                .text(format!("Model: {model}"))
                .build();
            if let Err(e) = bot.send_message(&params) {
                warn!("failed to send model selected to telegram: {e}");
            }
        }
        BridgeEvent::Unlocked => {
            let params = SendMessageParams::builder()
                .chat_id(chat_id)
                .text("Unlocked")
                .build();
            if let Err(e) = bot.send_message(&params) {
                warn!("failed to send unlocked message to telegram: {e}");
            }
        }
        BridgeEvent::Locked => {
            let params = SendMessageParams::builder()
                .chat_id(chat_id)
                .text("Locked")
                .build();
            if let Err(e) = bot.send_message(&params) {
                warn!("failed to send locked message to telegram: {e}");
            }
        }
        BridgeEvent::Pong => {
            let params = SendMessageParams::builder()
                .chat_id(chat_id)
                .text("pong")
                .build();
            if let Err(e) = bot.send_message(&params) {
                warn!("failed to send pong to telegram: {e}");
            }
        }
    }
}

fn send_photo_from_memory(bot: &Bot, chat_id: i64, data: &[u8]) -> Result<(), frankenstein::Error> {
    use std::io::Write;
    // Write image data to a temp file that is automatically cleaned up on drop.
    let mut tmp = tempfile::NamedTempFile::new().map_err(frankenstein::Error::ReadFile)?;
    tmp.write_all(data).map_err(frankenstein::Error::ReadFile)?;
    tmp.flush().map_err(frankenstein::Error::ReadFile)?;
    let path = tmp.path().to_path_buf();
    let params = SendPhotoParams::builder()
        .chat_id(chat_id)
        .photo(path)
        .build();
    bot.send_photo(&params).map(|_| ())
    // NamedTempFile is dropped here, removing the temp file automatically.
}

fn to_telegram_html(html: &str) -> String {
    let mut cleaner = HtmlSanitizer::new();
    cleaner.add_tags(&[
        "b", "strong", "i", "em", "u", "ins", "s", "strike", "del", "code", "pre", "a",
    ]);
    cleaner.add_generic_attributes(&["href"]);
    cleaner.clean(html).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allowed_tags_preserved() {
        let result = to_telegram_html("<b>bold</b>");
        assert!(result.contains("<b>bold</b>"));
    }

    #[test]
    fn test_disallowed_tags_stripped() {
        let result = to_telegram_html("<script>alert(1)</script>");
        assert!(!result.contains("<script>"));
        assert!(!result.contains("alert"));
    }

    #[test]
    fn test_allowed_attributes_preserved() {
        let result = to_telegram_html(r#"<a href="https://example.com">link</a>"#);
        assert!(result.contains(r#"href="https://example.com""#));
    }

    #[test]
    fn test_disallowed_attributes_stripped() {
        let result = to_telegram_html(r#"<a onclick="evil()">click</a>"#);
        assert!(result.contains("<a"));
        assert!(!result.contains("onclick"));
    }
}
