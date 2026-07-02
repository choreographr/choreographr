use std::sync::Arc;
use ammonia::Builder as HtmlSanitizer;
use tai_client_core::{ShellCommand, parse_input_line, render_markdown_html};
use tai_proto::ClientMessage;
use teloxide::{dispatching::UpdateFilterExt, prelude::*};
use teloxide::types::{ChatId, InputFile, ParseMode};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

use crate::bridge::{BridgeEvent, DaemonBridgeCommand};

pub async fn run(
    bot_token: String,
    admin_ids: Vec<i64>,
    bridge_tx: mpsc::Sender<DaemonBridgeCommand>,
    mut bridge_rx: mpsc::Receiver<BridgeEvent>,
) {
    let bot = Bot::new(&bot_token);

    let chat_id = Arc::new(Mutex::new(None::<ChatId>));

    {
        let bot = bot.clone();
        let chat_id = chat_id.clone();
        tokio::spawn(async move {
            while let Some(event) = bridge_rx.recv().await {
                debug!(?event, "bridge event received");
                let cid = *chat_id.lock().await;
                if let Some(cid) = cid {
                    send_daemon_event(&bot, cid, event).await;
                } else {
                    warn!("no chat id set, dropping bridge event");
                }
            }
            info!("bridge event stream ended");
        });
    }

    let state = Arc::new(TelegramState {
        bridge_tx,
        admin_ids,
        request_id: Mutex::new(0),
        chat_id: chat_id.clone(),
    });

    let handler = Update::filter_message().branch(
        dptree::entry()
            .filter_async(|msg: Message, state: Arc<TelegramState>| {
                let admin_ids = state.admin_ids.clone();
                async move { msg.chat.is_private() && admin_ids.contains(&msg.chat.id.0) }
            })
            .endpoint(handle_message),
    );

    info!("starting telegram bot dispatcher");
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
    info!("telegram bot dispatcher stopped");
}

struct TelegramState {
    bridge_tx: mpsc::Sender<DaemonBridgeCommand>,
    admin_ids: Vec<i64>,
    request_id: Mutex<u32>,
    chat_id: Arc<Mutex<Option<ChatId>>>,
}

async fn handle_message(
    bot: Bot,
    msg: Message,
    state: Arc<TelegramState>,
) -> ResponseResult<()> {
    let text = match msg.text() {
        Some(t) => t.to_string(),
        None => return Ok(()),
    };

    let user_id = msg.chat.id.0;
    debug!(%user_id, input = %text, "received telegram message");

    *state.chat_id.lock().await = Some(msg.chat.id);

    let mut request_id = *state.request_id.lock().await;
    let command = parse_input_line(&text, &mut request_id);
    *state.request_id.lock().await = request_id;

    match command {
        ShellCommand::Send(client_msg) => {
            if let ClientMessage::RunInput { input, .. } = &client_msg {
                let echo = format!("> {}", String::from_utf8_lossy(input));
                let _ = bot.send_message(msg.chat.id, echo).await;
            }
            debug!(request_id, "sending command to bridge");
            state
                .bridge_tx
                .send(DaemonBridgeCommand::SendMessage(client_msg))
                .await
                .ok();
        }
        ShellCommand::UnknownCommand(err) => {
            warn!(%err, "unknown command from user");
            let _ = bot.send_message(msg.chat.id, err).await;
        }
        ShellCommand::Empty | ShellCommand::InvalidCancel(_) => {}
    }

    Ok(())
}

async fn send_daemon_event(bot: &Bot, chat_id: ChatId, event: BridgeEvent) {
    match event {
        BridgeEvent::Text(text) => {
            let html = render_markdown_html(&text);
            let tg_html = to_telegram_html(&html);
            if !tg_html.is_empty()
                && let Err(e) = bot
                    .send_message(chat_id, tg_html)
                    .parse_mode(ParseMode::Html)
                    .await
            {
                error!(%e, "failed to send text message to telegram");
            }
        }
        BridgeEvent::ToolCallStarted {
            name,
            arguments_json,
        } => {
            let _ = bot
                .send_message(
                    chat_id,
                    format!("<i>Running {name} {arguments_json}</i>"),
                )
                .parse_mode(ParseMode::Html)
                .await;
        }
        BridgeEvent::ToolCallFinished { name, output } => {
            let _ = bot
                .send_message(chat_id, format!("<b>{name}</b>: {output}"))
                .parse_mode(ParseMode::Html)
                .await;
        }
        BridgeEvent::ToolCallFailed { name, error: error_msg } => {
            error!(%name, %error_msg, "tool call failed");
            let _ = bot
                .send_message(chat_id, format!("<b>{name}</b> failed: {error_msg}"))
                .parse_mode(ParseMode::Html)
                .await;
        }
        BridgeEvent::Image { data, .. } => {
            let input = InputFile::memory(data);
            if let Err(e) = bot.send_photo(chat_id, input).await {
                error!(%e, "failed to send image to telegram");
            }
        }
        BridgeEvent::Error(msg) => {
            error!(%msg, "sending error to telegram");
            let _ = bot.send_message(chat_id, msg).await;
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
            let _ = bot.send_message(chat_id, text).await;
        }
        BridgeEvent::ModelSelected(model) => {
            let _ = bot.send_message(chat_id, format!("Model: {model}")).await;
        }
        BridgeEvent::Unlocked => {
            let _ = bot.send_message(chat_id, "Unlocked").await;
        }
        BridgeEvent::Locked => {
            let _ = bot.send_message(chat_id, "Locked").await;
        }
        BridgeEvent::Pong => {
            let _ = bot.send_message(chat_id, "pong").await;
        }
    }
}

fn to_telegram_html(html: &str) -> String {
    let mut cleaner = HtmlSanitizer::new();
    cleaner.add_tags(&[
        "b", "strong", "i", "em", "u", "ins", "s", "strike", "del", "code", "pre", "a",
    ]);
    cleaner.add_generic_attributes(&["href"]);
    cleaner.clean(html).to_string()
}
