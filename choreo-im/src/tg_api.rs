//! Minimal Telegram Bot API client covering the three calls the bridge needs:
//! `getUpdates`, `sendMessage`, and `sendPhoto`.
//!
//! Hand-rolled over [`ureq`](https://docs.rs/ureq) rather than a full bot
//! framework — the bridge only needs long-polling, a JSON message send, and a
//! multipart photo upload, and the response shapes it decodes ([`Update`],
//! [`Message`], [`Chat`], [`User`]) are the subset it reads. Every call surfaces
//! failures as [`TelegramError`].

use std::io::Write;
use thiserror::Error;
use ureq::Agent;
use ureq::config::Config;

/// Failure returned by a [`Bot`] API call.
#[derive(Debug, Error)]
pub enum TelegramError {
    /// The HTTP request failed to send, or its response failed to parse.
    #[error("HTTP request failed: {0}")]
    Http(#[from] ureq::Error),
    /// An I/O error while assembling a request body.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The Bot API replied with `ok: false`.
    #[error("Telegram API error: {description}")]
    Api {
        /// The human-readable error description from the API.
        description: String,
    },
}

/// One entry from `getUpdates`: the poll cursor and the message it carried, if
/// any.
#[derive(serde::Deserialize, Debug)]
pub struct Update {
    /// Monotonic update id; passed back as the next poll offset.
    pub update_id: u32,
    /// The message payload, if this update carried one.
    pub message: Option<Message>,
}

/// A Telegram message, reduced to the fields the bridge reads.
#[derive(serde::Deserialize, Debug)]
pub struct Message {
    /// The chat the message belongs to.
    pub chat: Chat,
    /// The message text, if any (non-text messages are ignored).
    pub text: Option<String>,
    /// The sending user, if known.
    pub from: Option<User>,
}

/// The chat a [`Message`] belongs to.
#[derive(serde::Deserialize, Debug)]
pub struct Chat {
    /// The chat id, echoed back as the destination of outbound messages.
    pub id: i64,
    /// The chat type (e.g. `private`); only private chats are served.
    #[serde(rename = "type")]
    pub type_field: String,
}

/// The user who sent a [`Message`], reduced to the id used for the admin check.
#[derive(serde::Deserialize, Debug)]
pub struct User {
    /// The Telegram user id, matched against the configured admin list.
    pub id: i64,
}

#[derive(serde::Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

/// A Telegram Bot API client bound to one bot token.
///
/// Cloning shares the underlying [`ureq::Agent`] connection pool, so a clone is
/// cheap; that is how the polling loop and the event-render thread share one
/// client.
#[derive(Clone)]
pub struct Bot {
    token: String,
    agent: Agent,
}

impl Bot {
    /// Build a client for `token` (the bot token, i.e. the part after `bot` in
    /// the API URL).
    ///
    /// Uses a 10-second connect timeout and disables ureq's status-as-error
    /// behaviour, so the Bot API's JSON `ok`/`description` body can be read and
    /// mapped to [`TelegramError::Api`] regardless of HTTP status.
    #[must_use]
    pub fn new(token: &str) -> Self {
        Self {
            token: token.to_string(),
            agent: Agent::new_with_config(
                Config::builder()
                    .timeout_connect(Some(std::time::Duration::from_secs(10)))
                    .http_status_as_error(false)
                    .build(),
            ),
        }
    }

    /// Long-poll `getUpdates` for new messages after `offset`.
    ///
    /// `timeout` is the server-side long-poll hold, in seconds; Telegram
    /// returns an empty result once it elapses. `offset` is the id one past the
    /// last processed update, so already-acknowledged updates are dropped.
    ///
    /// # Errors
    ///
    /// Returns [`TelegramError::Http`] on request send/parse failure and
    /// [`TelegramError::Api`] when the Bot API returns `ok: false`.
    pub fn get_updates(&self, offset: u32, timeout: u32) -> Result<Vec<Update>, TelegramError> {
        let base = format!("https://api.telegram.org/bot{}/getUpdates", self.token);
        let body = serde_json::json!({
            "offset": i64::from(offset),
            "timeout": timeout,
        });
        let response = self.agent.post(&base).send_json(body)?;
        let api_resp: ApiResponse<Vec<Update>> = response.into_body().read_json()?;
        if !api_resp.ok {
            return Err(TelegramError::Api {
                description: api_resp.description.unwrap_or_default(),
            });
        }
        Ok(api_resp.result.unwrap_or_default())
    }

    /// Send a text message to `chat_id`.
    ///
    /// `parse_mode` (e.g. `Some("HTML")`) selects how Telegram interprets the
    /// markup in `text`; `None` sends it verbatim.
    ///
    /// # Errors
    ///
    /// Returns [`TelegramError::Http`] on network
    /// failure and [`TelegramError::Api`] when the Bot API returns `ok: false`.
    pub fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<(), TelegramError> {
        let base = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
        });
        if let Some(mode) = parse_mode {
            // Map::insert (instead of Value's IndexMut, which would panic on
            // a non-object body) is the correct API for setting top-level keys.
            if let Some(obj) = body.as_object_mut() {
                obj.insert("parse_mode".into(), serde_json::json!(mode));
            }
        }
        let response = self.agent.post(&base).send_json(body)?;
        let api_resp: ApiResponse<serde_json::Value> = response.into_body().read_json()?;
        if !api_resp.ok {
            return Err(TelegramError::Api {
                description: api_resp.description.unwrap_or_default(),
            });
        }
        Ok(())
    }

    /// Upload `data` as a photo to `chat_id` via a multipart `sendPhoto`.
    ///
    /// The image is sent inline (never as a URL), so bytes fetched on demand
    /// from the daemon are uploaded directly. The multipart boundary is made
    /// unique from the current time.
    ///
    /// # Errors
    ///
    /// Returns [`TelegramError::Http`] on network failure and
    /// [`TelegramError::Api`] when the Bot API returns `ok: false`.
    pub fn send_photo(&self, chat_id: i64, data: &[u8]) -> Result<(), TelegramError> {
        let base = format!("https://api.telegram.org/bot{}/sendPhoto", self.token);
        // Build a unique boundary for multipart/form-data.
        let boundary = format!(
            "----choreo{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );

        let mut body = Vec::new();
        write!(&mut body, "--{boundary}\r\n")?;
        write!(
            &mut body,
            "Content-Disposition: form-data; name=\"chat_id\"\r\n\r\n"
        )?;
        write!(&mut body, "{chat_id}\r\n")?;
        write!(&mut body, "--{boundary}\r\n")?;
        write!(
            &mut body,
            "Content-Disposition: form-data; name=\"photo\"; filename=\"image.png\"\r\n"
        )?;
        write!(&mut body, "Content-Type: application/octet-stream\r\n\r\n")?;
        body.extend_from_slice(data);
        write!(&mut body, "\r\n")?;
        write!(&mut body, "--{boundary}--\r\n")?;

        let content_type = format!("multipart/form-data; boundary={boundary}");
        let response = self
            .agent
            .post(&base)
            .header("Content-Type", &content_type)
            .send(body)?;
        let api_resp: ApiResponse<serde_json::Value> = response.into_body().read_json()?;
        if !api_resp.ok {
            return Err(TelegramError::Api {
                description: api_resp.description.unwrap_or_default(),
            });
        }
        Ok(())
    }
}
