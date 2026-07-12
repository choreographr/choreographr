use std::io::Write;
use thiserror::Error;
use ureq::Agent;
use ureq::config::Config;

#[derive(Debug, Error)]
pub enum TelegramError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] ureq::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Telegram API error: {description}")]
    Api { description: String },
}

#[derive(serde::Deserialize, Debug)]
pub struct Update {
    pub update_id: u32,
    pub message: Option<Message>,
}

#[derive(serde::Deserialize, Debug)]
pub struct Message {
    pub chat: Chat,
    pub text: Option<String>,
    pub from: Option<User>,
}

#[derive(serde::Deserialize, Debug)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub type_field: String,
}

#[derive(serde::Deserialize, Debug)]
pub struct User {
    pub id: i64,
}

#[derive(serde::Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Clone)]
pub struct Bot {
    token: String,
    agent: Agent,
}

impl Bot {
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

    pub fn get_updates(&self, offset: u32, timeout: u32) -> Result<Vec<Update>, TelegramError> {
        let base = format!("https://api.telegram.org/bot{}/getUpdates", self.token);
        let body = serde_json::json!({
            "offset": offset as i64,
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
            body["parse_mode"] = serde_json::json!(mode);
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

    pub fn send_photo(&self, chat_id: i64, data: &[u8]) -> Result<(), TelegramError> {
        let base = format!("https://api.telegram.org/bot{}/sendPhoto", self.token);
        // Build a unique boundary for multipart/form-data.
        let boundary = format!(
            "----tai{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
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
