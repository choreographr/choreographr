//! Unit tests for the OAuth login surface's state transitions that do not need
//! a live login worker (no network or thread): account resolution, the
//! one-time descriptor fetch, and cancellation guards.

use crate::test_util::test_app;
use choreo_proto::{AccountInfo, ClientMessage, ClientMessageType, OAuthProviderDescriptor};

fn account(name: &str, provider: &str) -> AccountInfo {
    AccountInfo {
        name: name.to_string(),
        provider: provider.to_string(),
        has_credential: false,
    }
}

fn descriptor(provider: &str) -> OAuthProviderDescriptor {
    OAuthProviderDescriptor {
        provider: provider.to_string(),
        issuer: "https://auth.example".to_string(),
        authorize_path: "/authorize".to_string(),
        token_path: "/token".to_string(),
        client_id: Some("client".to_string()),
        scopes: vec!["openid".to_string()],
        redirect_ports: vec![1455],
        code_exchange_encoding: choreo_proto::OAuthGrantEncoding::Form,
        refresh_encoding: choreo_proto::OAuthGrantEncoding::Json,
        client_identity: choreo_proto::OAuthClientIdentity::Registered,
        device_code: false,
        device_authorization_path: None,
        device_token_path: None,
    }
}

#[test]
fn begin_login_unknown_account_reports_and_sends_nothing() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded::<ClientMessage>();
    app.begin_login("nope".to_string(), false, &tx);
    assert!(
        app.status
            .as_deref()
            .is_some_and(|s| s.contains("no account"))
    );
    assert!(app.login.is_none());
    assert!(
        rx.try_recv().is_err(),
        "no request is sent for an unknown account"
    );
}

#[test]
fn begin_login_fetches_descriptors_once_and_defers_the_flow() {
    let mut app = test_app();
    app.ai_providers
        .set_accounts(vec![account("work", "openai")]);
    let (tx, rx) = crossbeam_channel::unbounded::<ClientMessage>();
    app.begin_login("work".to_string(), false, &tx);
    // The first `/login` requests the descriptors and stashes the pending flow.
    let sent = rx.recv().expect("ListOAuthProviders");
    assert_eq!(sent.inner, ClientMessageType::ListOAuthProviders);
    assert_eq!(
        app.pending_login,
        Some(("work".to_string(), "openai".to_string(), false))
    );
    assert!(app.login.is_none(), "the flow starts only after the reply");
}

#[test]
fn begin_login_reports_a_provider_without_oauth_support() {
    let mut app = test_app();
    app.ai_providers
        .set_accounts(vec![account("work", "anthropic")]);
    // Descriptors already fetched, but none for anthropic.
    app.oauth_providers_fetched = true;
    app.oauth_providers = vec![descriptor("openai")];
    let (tx, _rx) = crossbeam_channel::unbounded::<ClientMessage>();
    app.begin_login("work".to_string(), false, &tx);
    assert!(
        app.status
            .as_deref()
            .is_some_and(|s| s.contains("does not support OAuth")),
        "got: {:?}",
        app.status
    );
    assert!(app.login.is_none());
}

#[test]
fn handle_oauth_providers_reply_caches_without_pending() {
    let mut app = test_app();
    app.handle_oauth_providers_reply(vec![descriptor("openai")]);
    assert!(app.oauth_providers_fetched);
    assert_eq!(app.oauth_providers.len(), 1);
    assert!(app.pending_login.is_none());
    assert!(app.login.is_none());
}

#[test]
fn cancel_login_when_none_is_open_is_a_noop() {
    let mut app = test_app();
    assert!(!app.cancel_login());
}
