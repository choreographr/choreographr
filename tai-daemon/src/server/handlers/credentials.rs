use crate::openai::{OpenAiClient, load_service_config};
use crate::server::try_keystore_path;
use crate::tools::x;
use std::sync::Arc;
use tai_keystore::{Keystore, ServiceCredential};
use tai_proto::DaemonMessage;
use tokio::sync::mpsc;
use tracing::warn;

async fn send_or_warn(tx: &mpsc::Sender<DaemonMessage>, msg: DaemonMessage) {
    crate::server::send_or_warn(tx, msg).await;
}

pub(crate) async fn handle_unlock(
    state: &crate::DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    passphrase: String,
) -> anyhow::Result<()> {
    let Some(ks_path) = try_keystore_path(tx, |e| DaemonMessage::LockedError { error: e }).await?
    else {
        return Ok(());
    };
    if !ks_path.exists() {
        send_or_warn(tx, DaemonMessage::LockedError {
            error: "keystore does not exist. run 'tai-keystore init' to create one.".to_string(),
        }).await;
        return Ok(());
    }
    match Keystore::load(&ks_path, &passphrase) {
        Ok(ks) => {
            let keystore = Arc::new(ks);
            let mut guard = state.lock().await;
            match keystore.get_api_key("openai") {
                Some(api_key) => {
                    let service_config = match load_service_config() {
                        Ok(cfg) => cfg,
                        Err(e) => {
                            warn!(error = %anyhow::Error::from(e), "failed to load service config, using defaults — check config.toml");
                            Default::default()
                        }
                    };
                    match OpenAiClient::new(service_config, api_key.to_string()) {
                        Ok(client) => {
                            guard.openai_client = Some(Arc::new(client));
                            if let Some(x_creds) = keystore.get_x_credentials("twitter") {
                                x::set_x_credentials(x_creds);
                            }
                            guard.keystore = Some(keystore);
                            drop(guard);
                            send_or_warn(tx, DaemonMessage::Unlocked).await;
                        }
                        Err(e) => {
                            drop(guard);
                            send_or_warn(tx, DaemonMessage::LockedError {
                                error: format!("failed to create OpenAI client: {e}"),
                            }).await;
                        }
                    }
                }
                None => {
                    drop(guard);
                    send_or_warn(tx, DaemonMessage::LockedError {
                        error: "no 'openai' credential found in keystore".to_string(),
                    }).await;
                }
            }
        }
        Err(e) => {
            send_or_warn(tx, DaemonMessage::LockedError {
                error: format!("failed to unlock keystore: {e}"),
            }).await;
        }
    }
    Ok(())
}

pub(crate) async fn handle_lock(
    state: &crate::DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
) -> anyhow::Result<()> {
    let mut guard = state.lock().await;
    guard.openai_client = None;
    guard.keystore = None;
    drop(guard);
    x::clear_x_credentials();
    send_or_warn(tx, DaemonMessage::Locked).await;
    Ok(())
}

pub(crate) async fn handle_get_credential(
    state: &crate::DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    service: String,
) -> anyhow::Result<()> {
    let key = {
        let guard = state.lock().await;
        guard
            .keystore
            .as_ref()
            .and_then(|ks| ks.get_api_key(&service).map(|k| k.to_string()))
    };
    send_or_warn(tx, DaemonMessage::Credential { service, key }).await;
    Ok(())
}

pub(crate) async fn handle_add_api_key(
    tx: &mpsc::Sender<DaemonMessage>,
    service: String,
    passphrase: String,
    key: String,
) -> anyhow::Result<()> {
    let svc = service.clone();
    let Some(ks_path) = try_keystore_path(tx, |e| DaemonMessage::CredentialAddFailed {
        service: svc.clone(),
        error: e,
    })
    .await?
    else {
        return Ok(());
    };
    match Keystore::load(&ks_path, &passphrase) {
        Ok(mut keystore) => {
            keystore.add(svc.clone(), ServiceCredential::ApiKey { key });
            match keystore.save(&ks_path, &passphrase) {
                Ok(()) => {
                    send_or_warn(tx, DaemonMessage::CredentialAdded { service: svc }).await;
                }
                Err(e) => {
                    send_or_warn(tx, DaemonMessage::CredentialAddFailed {
                        service: svc,
                        error: format!("failed to save keystore: {e}"),
                    }).await;
                }
            }
        }
        Err(e) => {
            send_or_warn(tx, DaemonMessage::CredentialAddFailed {
                service: svc,
                error: format!("failed to unlock keystore: {e}"),
            }).await;
        }
    }
    Ok(())
}

pub(crate) async fn handle_add_x_credential(
    tx: &mpsc::Sender<DaemonMessage>,
    service: String,
    passphrase: String,
    api_key: String,
    api_key_secret: String,
    access_token: String,
    access_token_secret: String,
    bearer_token: Option<String>,
) -> anyhow::Result<()> {
    let svc = service.clone();
    let Some(ks_path) = try_keystore_path(tx, |e| DaemonMessage::CredentialAddFailed {
        service: svc.clone(),
        error: e,
    })
    .await?
    else {
        return Ok(());
    };
    match Keystore::load(&ks_path, &passphrase) {
        Ok(mut keystore) => {
            keystore.add(
                svc.clone(),
                ServiceCredential::X {
                    api_key,
                    api_key_secret,
                    access_token,
                    access_token_secret,
                    bearer_token,
                },
            );
            match keystore.save(&ks_path, &passphrase) {
                Ok(()) => {
                    send_or_warn(tx, DaemonMessage::CredentialAdded { service: svc }).await;
                }
                Err(e) => {
                    send_or_warn(tx, DaemonMessage::CredentialAddFailed {
                        service: svc,
                        error: format!("failed to save keystore: {e}"),
                    }).await;
                }
            }
        }
        Err(e) => {
            send_or_warn(tx, DaemonMessage::CredentialAddFailed {
                service: svc,
                error: format!("failed to unlock keystore: {e}"),
            }).await;
        }
    }
    Ok(())
}

pub(crate) async fn handle_remove_credential(
    tx: &mpsc::Sender<DaemonMessage>,
    service: String,
    passphrase: String,
) -> anyhow::Result<()> {
    let svc = service.clone();
    let Some(ks_path) = try_keystore_path(tx, |e| DaemonMessage::CredentialRemoveFailed {
        service: svc.clone(),
        error: e,
    })
    .await?
    else {
        return Ok(());
    };
    match Keystore::load(&ks_path, &passphrase) {
        Ok(mut keystore) => {
            if keystore.remove(&svc) {
                match keystore.save(&ks_path, &passphrase) {
                    Ok(()) => {
                        send_or_warn(tx, DaemonMessage::CredentialRemoved { service: svc }).await;
                    }
                    Err(e) => {
                        send_or_warn(tx, DaemonMessage::CredentialRemoveFailed {
                            service: svc,
                            error: format!("failed to save keystore: {e}"),
                        }).await;
                    }
                }
            } else {
                send_or_warn(tx, DaemonMessage::CredentialRemoveFailed {
                    service: svc,
                    error: "service not found in keystore".to_string(),
                }).await;
            }
        }
        Err(e) => {
            send_or_warn(tx, DaemonMessage::CredentialRemoveFailed {
                service: svc,
                error: format!("failed to unlock keystore: {e}"),
            }).await;
        }
    }
    Ok(())
}
