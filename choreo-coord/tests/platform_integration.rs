//! End-to-end integration tests for the Choreographr Coordination Platform.
//!
//! These exercise the real chain/indexer/IPFS boundary and are marked
//! `#[ignore]` (like every integration suite in the workspace) because they
//! require the local Coordination Platform services running:
//!   - the Substrate node (`choreo-runtime`) at `ws://127.0.0.1:9944`
//!   - the `acuity-index` indexer at `ws://127.0.0.1:8172` (with the
//!     `~/acuity-index/acuity.toml` spec)
//!   - the local IPFS daemon at `http://127.0.0.1:5001` (with a pinning
//!     service to propagate content)
//!
//! Run with `cargo nextest run -p choreo-coord --run-ignored all` once the
//! services are up. The suite seeds content end-to-end: encode a document,
//! pin it to IPFS, derive its item id, submit the `publish_item` extrinsic,
//! then read it back through the indexer + IPFS and verify the round-trip.
//!
//! A signing account is required; the test reads a Substrate credential from
//! the environment (`CHOREOGRAPHR_SUBSTRATE_JSON` + `CHOREOGRAPHR_SUBSTRATE_PASSWORD`)
//! or, failing that, imports the well-known dev "Alice" keystore. The account
//! must be funded (the dev chain funds Alice by default).

use choreo_coord::chain::{ChainAccount, account_id_from_address};
use choreo_coord::encode::{ContentInput, ContentType};
use choreo_coord::orchestrate;

/// A content digest hex that must not collide with the test's own upload.
fn test_digest_hex() -> String {
    format!("0x{}", "ab".repeat(32))
}

/// Build a `ChainAccount` for signing from a Substrate credential, defaulting
/// to the dev "Alice" account when none is supplied via the environment.
fn test_account() -> ChainAccount {
    if let (Ok(json), Ok(password)) = (
        std::env::var("CHOREOGRAPHR_SUBSTRATE_JSON"),
        std::env::var("CHOREOGRAPHR_SUBSTRATE_PASSWORD"),
    ) {
        let cred = choreo_keystore::substrate::import_from_json(&json, "test", &password).unwrap();
        let view = cred.as_substrate().unwrap();
        let account_id = account_id_from_address(view.account_id).unwrap();
        return ChainAccount::from_parts(account_id, view.secret.to_vec());
    }
    // Fall back to the well-known dev "Alice" keystore (password `whoisalice`).
    let alice = r#"{
        "encoded": "DumgApKCTqoCty1OZW/8WS+sgo6RdpHhCwAkA2IoDBMAgAAAAQAAAAgAAAB6IG/q24EeVf0JqWqcBd5m2tKq5BlyY84IQ8oamLn9DZe9Ouhgunr7i36J1XxUnTI801axqL/ym1gil0U8440Qvj0lFVKwGuxq38zuifgoj0B3Yru0CI6QKEvQPU5xxj4MpyxdSxP+2PnTzYao0HDH0fulaGvlAYXfqtU89xrx2/z9z7IjSwS3oDFPXRQ9kAdDebtyCVreZ9Otw9v3",
        "encoding": {"content": ["pkcs8","sr25519"], "type": ["scrypt","xsalsa20-poly1305"], "version": "3"},
        "address": "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY"
    }"#;
    let cred = choreo_keystore::substrate::import_from_json(alice, "alice", "whoisalice").unwrap();
    let view = cred.as_substrate().unwrap();
    let account_id = account_id_from_address(view.account_id).unwrap();
    ChainAccount::from_parts(account_id, view.secret.to_vec())
}

/// Round-trip a document through the platform: encode -> IPFS -> derive id ->
/// submit -> read back via indexer/IPFS -> verify content.
#[test]
#[ignore = "requires the local Coordination Platform services (node, indexer, IPFS)"]
fn publish_and_read_item_round_trip() {
    // Start the tokio sidecar (chain writes need it).
    choreo_coord::init().expect("coordinator runtime must initialize");

    let account = test_account();
    let content = ContentInput {
        content_type: ContentType::Document,
        title: Some("Integration Doc".into()),
        body: Some("body text".into()),
        language: Some("en".into()),
        image: None,
        profile: None,
    };

    // 1. Encode + pin to IPFS, derive the item id, submit.
    let outcome = orchestrate::publish_item(&account, &content, vec![], vec![], vec![], None, None)
        .expect("publish_item should succeed");
    let item_id_hex = outcome
        .item_id
        .expect("a PublishItem event must report the item id");

    // 2. Read it back through the indexer + IPFS, and decode the content.
    let resolved =
        orchestrate::item(&item_id_hex, None).expect("item should be resolvable via the indexer");
    assert_eq!(resolved.content.title.as_deref(), Some("Integration Doc"));
    assert_eq!(resolved.content.content_type, ContentType::Document);

    // 3. The on-chain state (owner/authority) is populated.
    assert!(!resolved.owner.is_empty());

    // 4. Publish a revision and confirm it supersedes.
    let revised = ContentInput {
        content_type: ContentType::Document,
        title: Some("Integration Doc v2".into()),
        body: None,
        language: None,
        image: None,
        profile: None,
    };
    let id = choreo_coord::encode::hex_to_bytes(&item_id_hex).unwrap();
    orchestrate::publish_revision(&account, id, &revised, vec![], vec![])
        .expect("publish_revision should succeed");
    let after = orchestrate::item(&item_id_hex, None).expect("revised item should be resolvable");
    assert_eq!(after.content.title.as_deref(), Some("Integration Doc v2"));
}

/// The indexer query path resolves an item's revision history.
#[test]
#[ignore = "requires the local Coordination Platform services (node, indexer, IPFS)"]
fn indexer_resolves_revision_history() {
    choreo_coord::init().expect("coordinator runtime must initialize");
    // Querying an arbitrary (likely absent) item id must not panic; it should
    // return an empty history or a clear error.
    let _ = orchestrate::revisions(&test_digest_hex());
}
