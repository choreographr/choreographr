use ammonia::Builder as HtmlSanitizer;
use choreo_client_core::{
    ShellCommand, build_add_credential_message, parse_input_line, resolve_private_key,
};
use choreo_markdown::render_markdown_html;
use choreo_proto::ClientMessage;
use std::cell::Cell;
use std::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::bridge::BridgeEvent;
use crate::tg_api::Bot;

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
        match bot.get_updates(update_id + 1, 10) {
            Ok(updates) => {
                for update in updates {
                    if let Some(msg) = update.message {
                        update_id = update.update_id;
                        handle_message(&bot, &state, msg);
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

fn is_chat_private(msg: &crate::tg_api::Message) -> bool {
    msg.chat.type_field == "private"
}

fn is_admin(msg: &crate::tg_api::Message, admin_ids: &[i64]) -> bool {
    msg.from
        .as_ref()
        .is_some_and(|user| admin_ids.contains(&user.id))
}

fn handle_message(bot: &Bot, state: &TelegramState, msg: crate::tg_api::Message) {
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
                if let Err(e) = bot.send_message(chat_id_val, &echo, None) {
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
            if let Err(e) = bot.send_message(chat_id_val, &err, None) {
                warn!("failed to send error message to telegram: {e}");
            }
        }
        ShellCommand::Unlock { method } => match resolve_private_key(&method) {
            Ok(private_key) => {
                if let Err(e) = state.bridge_tx.send(ClientMessage::Unlock { private_key }) {
                    warn!("failed to send unlock to bridge: {e}");
                }
            }
            Err(e) => {
                let _ = bot.send_message(chat_id_val, &format!("[error] {e}"), None);
            }
        },
        ShellCommand::AddCredential {
            service,
            credential_type,
            fields,
            unlock,
        } => match build_add_credential_message(service, credential_type, fields, unlock) {
            Ok(msg) => {
                if let Err(e) = state.bridge_tx.send(msg) {
                    warn!("failed to send add credential to bridge: {e}");
                }
            }
            Err(e) => {
                let _ = bot.send_message(chat_id_val, &format!("[error] {e}"), None);
            }
        },
        ShellCommand::RemoveCredential { service } => {
            if let Err(e) = state
                .bridge_tx
                .send(ClientMessage::RemoveCredential { service })
            {
                warn!("failed to send remove credential to bridge: {e}");
            }
        }
        ShellCommand::AclAdd { .. } => {
            // An IM bridge is inherently a REMOTE client: the daemon refuses
            // AclAdd from TCP/Noise connections, so do not even forward it.
            let _ = bot.send_message(
                chat_id_val,
                "[error] /acl add is only available from local connections",
                None,
            );
        }
        ShellCommand::Empty | ShellCommand::InvalidCancel(_) => {}
        ShellCommand::Undo | ShellCommand::Redo => {}
        ShellCommand::Continue | ShellCommand::Stop => {
            debug!("telegram does not support Continue/Stop commands");
        }
        ShellCommand::RefreshModels { force } => {
            // The daemon refresh is client-agnostic: forward the request over
            // the bridge like any other Send command.
            if let Err(e) = state.bridge_tx.send(ClientMessage::RefreshModels { force }) {
                warn!("failed to send refresh-models to bridge: {e}");
            }
        }
    }
}

fn send_daemon_event(bot: &Bot, chat_id: i64, event: BridgeEvent) {
    match event {
        BridgeEvent::Text(text) => {
            let html = render_markdown_html(&text);
            let tg_html = to_telegram_html(&html);
            if !tg_html.is_empty()
                && let Err(e) = bot.send_message(chat_id, &tg_html, Some("HTML"))
            {
                error!(%e, "failed to send text message to telegram");
            }
        }
        BridgeEvent::ToolCallStarted {
            name,
            arguments_json,
        } => {
            if let Err(e) = bot.send_message(
                chat_id,
                &format!("<i>Running {name} {arguments_json}</i>"),
                Some("HTML"),
            ) {
                warn!("failed to send tool call started message: {e}");
            }
        }
        BridgeEvent::ToolCallFinished { name, output } => {
            if let Err(e) =
                bot.send_message(chat_id, &format!("<b>{name}</b>: {output}"), Some("HTML"))
            {
                warn!("failed to send tool call finished message: {e}");
            }
        }
        BridgeEvent::ToolCallFailed {
            name,
            error: error_msg,
        } => {
            error!(%name, %error_msg, "tool call failed");
            if let Err(e) = bot.send_message(
                chat_id,
                &format!("<b>{name}</b> failed: {error_msg}"),
                Some("HTML"),
            ) {
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
            if let Err(e) = bot.send_message(chat_id, &msg, None) {
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
            if let Err(e) = bot.send_message(chat_id, &text, None) {
                warn!("failed to send models list to telegram: {e}");
            }
        }
        BridgeEvent::ModelSelected(model) => {
            if let Err(e) = bot.send_message(chat_id, &format!("Model: {model}"), None) {
                warn!("failed to send model selected to telegram: {e}");
            }
        }
        BridgeEvent::Unlocked => {
            if let Err(e) = bot.send_message(chat_id, "Unlocked", None) {
                warn!("failed to send unlocked message to telegram: {e}");
            }
        }
        BridgeEvent::Locked => {
            if let Err(e) = bot.send_message(chat_id, "Locked", None) {
                warn!("failed to send locked message to telegram: {e}");
            }
        }
        BridgeEvent::Pong => {
            if let Err(e) = bot.send_message(chat_id, "pong", None) {
                warn!("failed to send pong to telegram: {e}");
            }
        }
    }
}

fn send_photo_from_memory(
    bot: &Bot,
    chat_id: i64,
    data: &[u8],
) -> Result<(), crate::tg_api::TelegramError> {
    bot.send_photo(chat_id, data)
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
