//! Thin `Tool` trait wrappers over the Choreographr Coordination Platform.
//!
//! The actual blocking operations live in the `choreo-coord` crate (which owns
//! the tokio sidecar runtime that drives `subxt` for chain writes and the
//! `ureq`/`tungstenite` synchronously for IPFS and the indexer). This module
//! only adapts them to the daemon's `Tool` trait so `choreo-daemon` never
//! depends on subxt or tokio directly.
//!
//! ## Credentials
//!
//! The write tools need a signing [`ChainAccount`], built from the daemon's
//! single Substrate credential. That credential travels through the `Tool`
//! trait's `x_credentials` slot (see the `// TEMPORARY:` notes in
//! `requests.rs`/`sessions.rs` — this single-slot reuse is a stopgap until a
//! proper tool→keystore credential-access system replaces it). Every write
//! tool's `execute` builds the account from that credential and errors with a
//! [`ToolExecError`] when no Substrate credential is present. The read tools
//! need no credential and ignore it.

use crate::tools::{Tool, ToolExecError};
use choreo_coord::chain::ChainAccount;
use choreo_coord::encode::{ContentInput, bytes_to_hex, hex_to_bytes};
use choreo_coord::indexer::QueryKey;
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The single `"coord"` group name shared by every tool in this module.
const GROUP: &str = "coord";

/// A shared `impl Tool` generator for the coord tools.
///
/// Every coord tool returns a `String` (a pre-formatted readable result
/// produced inside `execute`) and needs the raw `x_credentials` (write tools
/// build a [`ChainAccount`] from it). This mirrors `define_tool!` in
/// `tools/mod.rs` but forwards the credential instead of discarding it.
macro_rules! coord_tool {
    ($struct:ident, $name:literal, $desc:literal, $args:ty, $exec:expr, $invoke:expr) => {
        impl Tool for $struct {
            type Args = $args;
            type Return = String;
            type Error = ToolExecError;

            fn name(&self) -> &'static str {
                $name
            }
            fn group(&self) -> &'static str {
                GROUP
            }
            fn description(&self) -> &'static str {
                $desc
            }
            fn execute(
                &self,
                args: Self::Args,
                x_credentials: Option<&ServiceCredential>,
                working_dir: Option<&Path>,
                _ctx: Option<&crate::tools::context::ToolContext>,
            ) -> Result<Self::Return, Self::Error> {
                ($exec)(&args, x_credentials, working_dir)
            }
            fn return_string(ret: &Self::Return) -> String {
                ret.clone()
            }
            fn describe_invocation(&self, args: &Self::Args) -> String {
                ($invoke)(args)
            }
        }
    };
}

// ── ChainAccount construction ───────────────────────────────────────────────

/// Borrow the Substrate credential fields from the daemon-provided credential.
///
/// Returns a [`ToolExecError`] when no credential is present or it is not a
/// Substrate (Polkadot) account.
fn substrate_view(
    cred: Option<&ServiceCredential>,
) -> Result<choreo_keystore::SubstrateCredentialView<'_>, ToolExecError> {
    let cred = cred.ok_or_else(|| {
        ToolExecError(
            "no Substrate credential configured for the Coordination Platform; \
             save a Substrate account credential and unlock the daemon first"
                .into(),
        )
    })?;
    cred.as_substrate()
        .ok_or_else(|| ToolExecError("the configured credential is not a Substrate account".into()))
}

/// Extract the raw 32-byte account id from a Substrate credential view.
fn credential_public(
    view: &choreo_keystore::SubstrateCredentialView<'_>,
) -> Result<[u8; 32], ToolExecError> {
    view.public
        .try_into()
        .map_err(|_| ToolExecError("Substrate credential public key is not 32 bytes".into()))
}

/// Extract a signing [`ChainAccount`] from a Substrate credential.
///
/// Builds the account from the stored `public` (32 bytes) + `secret` (64 bytes)
/// via [`ChainAccount::from_parts`]. Errors (a [`ToolExecError`]) when no
/// Substrate credential is present or it is the wrong shape.
fn chain_account_from_credential(
    cred: Option<&ServiceCredential>,
) -> Result<ChainAccount, ToolExecError> {
    let view = substrate_view(cred)?;
    Ok(ChainAccount::from_parts(
        credential_public(&view)?,
        view.secret.to_vec(),
    ))
}

/// Build a signing [`ChainAccount`] for a write tool, validating the caller's
/// `account` label against the credential's secret.
///
/// When `account` is non-empty, [`ChainAccount::from_address`] is used so the
/// address and the secret are checked to agree — a mismatch surfaces as an
/// error rather than silently signing with the wrong key. When `account` is
/// empty (the caller did not specify which account to act as), fall back to
/// [`ChainAccount::from_parts`] using the credential's own public key.
fn chain_account_for_write(
    cred: Option<&ServiceCredential>,
    account: &str,
) -> Result<ChainAccount, ToolExecError> {
    if account.trim().is_empty() {
        return chain_account_from_credential(cred);
    }
    let view = substrate_view(cred)?;
    ChainAccount::from_address(account, view.secret.to_vec())
        .map_err(|e| ToolExecError(e.to_string()))
}

// ── Argument parsing helpers ───────────────────────────────────────────────

/// Parse a list of `0x` hex strings into raw 32-byte ids.
fn parse_id_list(values: &[String]) -> Result<Vec<[u8; 32]>, ToolExecError> {
    values
        .iter()
        .map(|v| hex_to_bytes(v).map_err(|e| ToolExecError(e.to_string())))
        .collect()
}

/// Parse an optional `0x` hex string into an optional raw 32-byte id.
fn parse_optional_id(value: &Option<String>) -> Result<Option<[u8; 32]>, ToolExecError> {
    value
        .as_deref()
        .map(|v| hex_to_bytes(v).map_err(|e| ToolExecError(e.to_string())))
        .transpose()
}

/// Map an [`EventsKey`] to a [`QueryKey`] for the indexer.
fn events_key_to_query(key: &EventsKey) -> Result<QueryKey, ToolExecError> {
    match key {
        EventsKey::ItemId { item_id } => Ok(QueryKey::ItemId(
            hex_to_bytes(item_id).map_err(|e| ToolExecError(e.to_string()))?,
        )),
        EventsKey::AccountId { account_id } => Ok(QueryKey::AccountId(
            hex_to_bytes(account_id).map_err(|e| ToolExecError(e.to_string()))?,
        )),
        EventsKey::IpfsHash { ipfs_hash } => Ok(QueryKey::IpfsHash(
            hex_to_bytes(ipfs_hash).map_err(|e| ToolExecError(e.to_string()))?,
        )),
        EventsKey::ItemRevision {
            item_id,
            revision_id,
        } => Ok(QueryKey::ItemRevision {
            item_id: hex_to_bytes(item_id).map_err(|e| ToolExecError(e.to_string()))?,
            revision_id: *revision_id,
        }),
    }
}

/// The query-key discriminator for `coord_events`, mirrored from the indexer's
/// known `QueryKey` variants.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum EventsKey {
    /// Exact `item_id` (32 bytes) match.
    ItemId {
        /// `0x`-prefixed 32-byte item id.
        item_id: String,
    },
    /// Exact `account_id` (32 bytes) match.
    AccountId {
        /// `0x`-prefixed 32-byte account id.
        account_id: String,
    },
    /// Exact `ipfs_hash` (32 bytes) match.
    IpfsHash {
        /// `0x`-prefixed 32-byte content digest.
        ipfs_hash: String,
    },
    /// Composite `item_id` + `revision_id` match.
    ItemRevision {
        /// `0x`-prefixed 32-byte item id.
        item_id: String,
        /// The on-chain revision counter.
        revision_id: u32,
    },
}

// ── Read tools ──────────────────────────────────────────────────────────────

pub(crate) struct CoordItem;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordItemArgs {
    /// `0x`-prefixed 32-byte item id.
    pub item_id: String,
    /// Specific revision to resolve; `None` resolves the latest.
    #[serde(default)]
    pub revision_id: Option<u32>,
}

fn execute_coord_item(
    args: &CoordItemArgs,
    _cred: Option<&ServiceCredential>,
    _wd: Option<&Path>,
) -> Result<String, ToolExecError> {
    let result = choreo_coord::orchestrate::item(&args.item_id, args.revision_id)?;
    Ok(format!(
        "item {}\n{}",
        result.item_id,
        format_resolved_item(&result)
    ))
}

coord_tool!(
    CoordItem,
    "coord_item",
    "Resolve a single item from the Choreographr Coordination Platform: its decoded content (latest or a specific revision), the revision id, IPFS hash, on-chain owner, and lifecycle flags.",
    CoordItemArgs,
    execute_coord_item,
    |args: &CoordItemArgs| format!("read item {} from the coordination platform", args.item_id)
);

pub(crate) struct CoordRevisions;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordRevisionsArgs {
    /// `0x`-prefixed 32-byte item id.
    pub item_id: String,
}

fn execute_coord_revisions(
    args: &CoordRevisionsArgs,
    _cred: Option<&ServiceCredential>,
    _wd: Option<&Path>,
) -> Result<String, ToolExecError> {
    let result = choreo_coord::orchestrate::revisions(&args.item_id)?;
    Ok(format_revision_entries(&result))
}

coord_tool!(
    CoordRevisions,
    "coord_revisions",
    "List the full revision history of an item on the Choreographr Coordination Platform, newest-first (revision counter, IPFS hash, block height, timestamp).",
    CoordRevisionsArgs,
    execute_coord_revisions,
    |args: &CoordRevisionsArgs| format!(
        "list revisions of item {} from the coordination platform",
        args.item_id
    )
);

pub(crate) struct CoordEvents;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordEventsArgs {
    /// The indexer query key selecting which events to read.
    pub key: EventsKey,
    /// Maximum number of events to return (newest-first).
    #[serde(default)]
    pub limit: Option<u16>,
    /// Cursor `(block_number, event_index)` to read strictly before.
    #[serde(default)]
    pub before: Option<(u32, u32)>,
}

fn execute_coord_events(
    args: &CoordEventsArgs,
    _cred: Option<&ServiceCredential>,
    _wd: Option<&Path>,
) -> Result<String, ToolExecError> {
    let key = events_key_to_query(&args.key)?;
    let result = choreo_coord::orchestrate::events(&key, args.limit.unwrap_or(1024), args.before)?;
    Ok(format_decoded_events(&result))
}

coord_tool!(
    CoordEvents,
    "coord_events",
    "Query the Choreographr Coordination Platform event indexer for events matching a key (item id, account id, IPFS hash, or item revision), newest-first, up to a limit.",
    CoordEventsArgs,
    execute_coord_events,
    |args: &CoordEventsArgs| format!("query coordination-platform events by {:?}", args.key)
);

pub(crate) struct CoordAccountItems;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordAccountItemsArgs {
    /// SS58 address of the account.
    pub account: String,
}

fn execute_coord_account_items(
    args: &CoordAccountItemsArgs,
    _cred: Option<&ServiceCredential>,
    _wd: Option<&Path>,
) -> Result<String, ToolExecError> {
    let result = choreo_coord::orchestrate::account_items(&args.account)?;
    Ok(format_account_items(&result))
}

coord_tool!(
    CoordAccountItems,
    "coord_account_items",
    "List the content items an account has pinned on the Choreographr Coordination Platform, resolving each item's title where possible.",
    CoordAccountItemsArgs,
    execute_coord_account_items,
    |args: &CoordAccountItemsArgs| format!(
        "list coordination-platform items for account {}",
        args.account
    )
);

pub(crate) struct CoordProfile;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordProfileArgs {
    /// SS58 address of the account.
    pub account: String,
}

fn execute_coord_profile(
    args: &CoordProfileArgs,
    _cred: Option<&ServiceCredential>,
    _wd: Option<&Path>,
) -> Result<String, ToolExecError> {
    let result = choreo_coord::orchestrate::profile(&args.account)?;
    Ok(format_profile(&result))
}

coord_tool!(
    CoordProfile,
    "coord_profile",
    "Resolve an account's profile on the Choreographr Coordination Platform: its profile item id and decoded name, bio, location, and account type.",
    CoordProfileArgs,
    execute_coord_profile,
    |args: &CoordProfileArgs| format!(
        "read profile for account {} from the coordination platform",
        args.account
    )
);

pub(crate) struct CoordDecodeContent;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordDecodeContentArgs {
    /// IPFS content reference: a `0x` digest hex or a Base58 CIDv0.
    pub content_ref: String,
}

fn execute_coord_decode_content(
    args: &CoordDecodeContentArgs,
    _cred: Option<&ServiceCredential>,
    _wd: Option<&Path>,
) -> Result<String, ToolExecError> {
    let result = choreo_coord::orchestrate::decode_content(&args.content_ref)?;
    Ok(format_decoded_content(&result))
}

coord_tool!(
    CoordDecodeContent,
    "coord_decode_content",
    "Decode arbitrary content bytes from the Coordination Platform IPFS store (by digest hex or CID) into the structured title/body/language/image/profile fields — no chain interaction.",
    CoordDecodeContentArgs,
    execute_coord_decode_content,
    |args: &CoordDecodeContentArgs| format!("decode content {} from IPFS", args.content_ref)
);

pub(crate) struct CoordStatus;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordStatusArgs {}

fn execute_coord_status(
    _args: &CoordStatusArgs,
    _cred: Option<&ServiceCredential>,
    _wd: Option<&Path>,
) -> Result<String, ToolExecError> {
    let result = choreo_coord::orchestrate::status()?;
    Ok(format_status(&result))
}

coord_tool!(
    CoordStatus,
    "coord_status",
    "Report the aggregate status of the Choreographr Coordination Platform services: chain, event indexer, and IPFS.",
    CoordStatusArgs,
    execute_coord_status,
    |_args: &CoordStatusArgs| "report Choreographr Coordination Platform status".to_string()
);

// ── Write tools ─────────────────────────────────────────────────────────────

pub(crate) struct CoordPublishItem;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordPublishItemArgs {
    /// SS58 address of the publishing account (validated against the credential's secret).
    pub account: String,
    /// The structured content to encode and publish.
    pub content: ContentInput,
    /// `0x` hex parent item ids.
    #[serde(default)]
    pub parents: Vec<String>,
    /// `0x` hex linked item ids.
    #[serde(default)]
    pub links: Vec<String>,
    /// `0x` hex mentioned account ids.
    #[serde(default)]
    pub mentions: Vec<String>,
    /// Lifecycle flag bitmask; `None` uses the default (revisionable + retractable).
    #[serde(default)]
    pub flags: Option<u8>,
    /// `0x` hex nonce; `None` generates a fresh one.
    #[serde(default)]
    pub nonce: Option<String>,
}

fn execute_coord_publish_item(
    args: &CoordPublishItemArgs,
    cred: Option<&ServiceCredential>,
    _wd: Option<&Path>,
) -> Result<String, ToolExecError> {
    let account = chain_account_for_write(cred, &args.account)?;
    let parents = parse_id_list(&args.parents)?;
    let links = parse_id_list(&args.links)?;
    let mentions = parse_id_list(&args.mentions)?;
    let nonce = parse_optional_id(&args.nonce)?;
    let outcome = choreo_coord::orchestrate::publish_item(
        &account,
        &args.content,
        parents,
        links,
        mentions,
        args.flags,
        nonce,
    )?;
    Ok(format_publish_outcome(&outcome))
}

coord_tool!(
    CoordPublishItem,
    "coord_publish_item",
    "Publish a brand-new item to the Choreographr Coordination Platform: encode the content, upload it to IPFS, derive the item id, and submit the chain extrinsic.",
    CoordPublishItemArgs,
    execute_coord_publish_item,
    |args: &CoordPublishItemArgs| format!(
        "publish a new coordination-platform item as {}",
        args.account
    )
);

pub(crate) struct CoordPublishRevision;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordPublishRevisionArgs {
    /// SS58 address of the publishing account.
    pub account: String,
    /// `0x` hex item id being revised.
    pub item_id: String,
    /// The content of the new revision.
    pub content: ContentInput,
    /// `0x` hex linked item ids.
    #[serde(default)]
    pub links: Vec<String>,
    /// `0x` hex mentioned account ids.
    #[serde(default)]
    pub mentions: Vec<String>,
}

fn execute_coord_publish_revision(
    args: &CoordPublishRevisionArgs,
    cred: Option<&ServiceCredential>,
    _wd: Option<&Path>,
) -> Result<String, ToolExecError> {
    let account = chain_account_for_write(cred, &args.account)?;
    let item_id = hex_to_bytes(&args.item_id).map_err(|e| ToolExecError(e.to_string()))?;
    let links = parse_id_list(&args.links)?;
    let mentions = parse_id_list(&args.mentions)?;
    let outcome = choreo_coord::orchestrate::publish_revision(
        &account,
        item_id,
        &args.content,
        links,
        mentions,
    )?;
    Ok(format_publish_outcome(&outcome))
}

coord_tool!(
    CoordPublishRevision,
    "coord_publish_revision",
    "Publish a new revision of an existing item on the Choreographr Coordination Platform: encode the content, upload to IPFS, and submit the chain extrinsic.",
    CoordPublishRevisionArgs,
    execute_coord_publish_revision,
    |args: &CoordPublishRevisionArgs| format!(
        "publish a new revision of item {} as {}",
        args.item_id, args.account
    )
);

pub(crate) struct CoordLifecycle;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordLifecycleArgs {
    /// SS58 address of the authorizing account.
    pub account: String,
    /// The lifecycle state transition to apply.
    pub action: choreo_coord::orchestrate::LifecycleAction,
    /// `0x` hex item id.
    pub item_id: String,
}

fn execute_coord_lifecycle(
    args: &CoordLifecycleArgs,
    cred: Option<&ServiceCredential>,
    _wd: Option<&Path>,
) -> Result<String, ToolExecError> {
    let account = chain_account_for_write(cred, &args.account)?;
    let item_id = hex_to_bytes(&args.item_id).map_err(|e| ToolExecError(e.to_string()))?;
    choreo_coord::orchestrate::lifecycle(&account, args.action, item_id)?;
    Ok(format!(
        "applied lifecycle action to item {}",
        bytes_to_hex(&item_id)
    ))
}

coord_tool!(
    CoordLifecycle,
    "coord_lifecycle",
    "Apply a lifecycle state transition to an item on the Choreographr Coordination Platform (retract, freeze as not-revisionable, or freeze as not-retractable).",
    CoordLifecycleArgs,
    execute_coord_lifecycle,
    |args: &CoordLifecycleArgs| format!(
        "apply {:?} lifecycle to item {}",
        args.action, args.item_id
    )
);

pub(crate) struct CoordAccountLink;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordAccountLinkArgs {
    /// SS58 address of the account.
    pub account: String,
    /// Whether to add (pin) or remove (unpin) the item.
    pub action: choreo_coord::orchestrate::AccountLinkAction,
    /// `0x` hex item id.
    pub item_id: String,
}

fn execute_coord_account_link(
    args: &CoordAccountLinkArgs,
    cred: Option<&ServiceCredential>,
    _wd: Option<&Path>,
) -> Result<String, ToolExecError> {
    let account = chain_account_for_write(cred, &args.account)?;
    let item_id = hex_to_bytes(&args.item_id).map_err(|e| ToolExecError(e.to_string()))?;
    choreo_coord::orchestrate::account_link(&account, args.action, item_id)?;
    Ok(format!(
        "{} item {} {} account",
        match args.action {
            choreo_coord::orchestrate::AccountLinkAction::Add => "pinned",
            choreo_coord::orchestrate::AccountLinkAction::Remove => "unpinned",
        },
        bytes_to_hex(&item_id),
        args.account,
    ))
}

coord_tool!(
    CoordAccountLink,
    "coord_account_link",
    "Pin (add) or unpin (remove) an item to/from an account on the Choreographr Coordination Platform.",
    CoordAccountLinkArgs,
    execute_coord_account_link,
    |args: &CoordAccountLinkArgs| format!(
        "{:?} item {} {} account",
        args.action, args.item_id, args.account
    )
);

pub(crate) struct CoordSetProfile;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoordSetProfileArgs {
    /// SS58 address of the account.
    pub account: String,
    /// The profile content (title/name, bio, location, account type).
    pub content: ContentInput,
}

fn execute_coord_set_profile(
    args: &CoordSetProfileArgs,
    cred: Option<&ServiceCredential>,
    _wd: Option<&Path>,
) -> Result<String, ToolExecError> {
    let account = chain_account_for_write(cred, &args.account)?;
    let outcome = choreo_coord::orchestrate::set_profile(&account, &args.content)?;
    Ok(format_publish_outcome(&outcome))
}

coord_tool!(
    CoordSetProfile,
    "coord_set_profile",
    "Set an account's profile on the Choreographr Coordination Platform: publish the profile item and point the account's profile at it.",
    CoordSetProfileArgs,
    execute_coord_set_profile,
    |args: &CoordSetProfileArgs| format!("set profile for account {}", args.account)
);

// ── Result formatting ───────────────────────────────────────────────────────

fn format_resolved_item(item: &choreo_coord::orchestrate::ResolvedItem) -> String {
    format!(
        "item_id: {}\nrevision_id: {}\nipfs_hash: {}\nowner: {}\nflags: {:#04x}\ncontent:\n{}",
        item.item_id,
        item.revision_id,
        item.ipfs_hash_hex,
        item.owner,
        item.flags,
        indent_block(&format_decoded_content(&item.content), 2),
    )
}

fn format_revision_entries(entries: &[choreo_coord::orchestrate::RevisionEntry]) -> String {
    if entries.is_empty() {
        return "no indexed revisions".to_string();
    }
    let mut out = String::new();
    for entry in entries {
        out.push_str(&format!(
            "revision {}: ipfs_hash={} block={} timestamp={}\n",
            entry.revision_id,
            entry.ipfs_hash_hex,
            entry
                .block_number
                .map_or_else(|| "?".into(), |b| b.to_string()),
            entry
                .timestamp
                .map_or_else(|| "?".into(), |t| t.to_string()),
        ));
    }
    out
}

fn format_decoded_events(events: &[choreo_coord::indexer::DecodedEvent]) -> String {
    if events.is_empty() {
        return "no matching events".to_string();
    }
    let mut out = format!("{} event(s):\n", events.len());
    for ev in events {
        out.push_str(&format!(
            "block {} event_index {}: {}::{} fields={}\n",
            ev.block_number,
            ev.event_index,
            ev.event.pallet_name,
            ev.event.event_name,
            ev.event.fields,
        ));
    }
    out
}

fn format_account_items(items: &[choreo_coord::orchestrate::AccountItem]) -> String {
    if items.is_empty() {
        return "no pinned items".to_string();
    }
    let mut out = String::new();
    for item in items {
        match &item.title {
            Some(t) => out.push_str(&format!("{} (title: {})\n", item.item_id, t)),
            None => out.push_str(&format!("{}\n", item.item_id)),
        }
    }
    out
}

fn format_profile(profile: &choreo_coord::orchestrate::ProfileResult) -> String {
    if !profile.exists {
        return "no profile set".to_string();
    }
    let mut out = format!(
        "profile item: {}\n",
        profile.item_id.as_deref().unwrap_or("-")
    );
    if let Some(name) = &profile.name {
        out.push_str(&format!("name: {name}\n"));
    }
    if let Some(bio) = &profile.bio {
        out.push_str(&format!("bio: {bio}\n"));
    }
    if let Some(location) = &profile.location {
        out.push_str(&format!("location: {location}\n"));
    }
    if let Some(account_type) = profile.account_type {
        out.push_str(&format!("account_type: {account_type}\n"));
    }
    out
}

fn format_decoded_content(content: &choreo_coord::encode::DecodedItem) -> String {
    let mut out = format!("content_type: {:?}\n", content.content_type);
    if let Some(title) = &content.title {
        out.push_str(&format!("title: {title}\n"));
    }
    if let Some(body) = &content.body {
        out.push_str(&format!("body: {body}\n"));
    }
    if let Some(language) = &content.language {
        out.push_str(&format!("language: {language}\n"));
    }
    if let Some(image) = &content.image {
        out.push_str(&format!(
            "image: filename={} filesize={} digest_hex={} {}x{}\n",
            image.filename, image.filesize, image.digest_hex, image.width, image.height,
        ));
    }
    if let Some(profile) = &content.profile {
        out.push_str(&format!(
            "profile: account_type={} location={}\n",
            profile.account_type, profile.location,
        ));
    }
    out
}

fn format_status(status: &choreo_coord::orchestrate::CoordStatus) -> String {
    let mut out = String::new();
    match &status.chain {
        Some(chain) => out.push_str(&format!(
            "chain: genesis={} ss58_prefix={} best_block={} finalized_block={} item_id_namespace={}\n",
            chain.genesis_hash,
            chain.ss58_prefix,
            chain.best_block,
            chain.finalized_block,
            chain.item_id_namespace,
        )),
        None => out.push_str("chain: unavailable\n"),
    }
    match &status.indexer {
        Some(indexer) => {
            out.push_str(&format!("indexer: {} span(s)\n", indexer.spans.len()));
            for span in &indexer.spans {
                out.push_str(&format!("  {}..{}\n", span.start, span.end));
            }
        }
        None => out.push_str("indexer: unavailable\n"),
    }
    match &status.ipfs {
        Some(ipfs) => out.push_str(&format!("ipfs: peer={}\n", ipfs.peer_id)),
        None => out.push_str("ipfs: unavailable\n"),
    }
    out
}

fn format_publish_outcome(outcome: &choreo_coord::chain::TxOutcome) -> String {
    match &outcome.item_id {
        Some(id) => format!("published; item_id={id}"),
        None => "published".to_string(),
    }
}

/// Indent every line of `block` by `width` spaces (used to nest decoded
/// content under a `content:` header).
fn indent_block(block: &str, width: usize) -> String {
    let pad = " ".repeat(width);
    block
        .lines()
        .map(|l| format!("{pad}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use choreo_keystore::ServiceCredential;

    /// Build a synthetic Substrate credential with a valid 64-byte expanded
    /// secret and a matching 32-byte public key (used by the read-side helper
    /// and by `chain_account_from_credential`, which never needs the address
    /// to cryptographically agree with the secret).
    fn substrate_credential() -> ServiceCredential {
        ServiceCredential::Substrate {
            name: "main".into(),
            account_id: "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY".into(),
            secret: vec![0x21; 64],
            public: vec![0x22; 32],
        }
    }

    #[test]
    fn chain_account_from_credential_success() {
        let cred = substrate_credential();
        let account = chain_account_from_credential(Some(&cred)).unwrap();
        assert_eq!(account.account_id, [0x22u8; 32]);
        // The public key round-trips through the account's address form.
        assert!(!account.address.is_empty());
    }

    #[test]
    fn chain_account_from_credential_none_errors() {
        let err = match chain_account_from_credential(None) {
            Err(e) => e,
            Ok(_) => panic!("expected an error for a missing credential"),
        };
        assert!(err.to_string().contains("Substrate credential"));
    }

    #[test]
    fn chain_account_from_credential_non_substrate_errors() {
        let cred = ServiceCredential::ApiKey {
            key: "sk-test".into(),
        };
        let err = match chain_account_from_credential(Some(&cred)) {
            Err(e) => e,
            Ok(_) => panic!("expected an error for a non-Substrate credential"),
        };
        assert!(err.to_string().contains("not a Substrate account"));
    }

    #[test]
    fn chain_account_for_write_empty_account_falls_back_to_from_parts() {
        // Empty/missing account label -> from_parts (no address validation).
        let cred = substrate_credential();
        let account = chain_account_for_write(Some(&cred), "").unwrap();
        assert_eq!(account.account_id, [0x22u8; 32]);
    }

    #[test]
    fn events_key_maps_to_query_keys() {
        let id = format!("0x{}", "11".repeat(32));
        let key = EventsKey::ItemId {
            item_id: id.clone(),
        };
        match events_key_to_query(&key).unwrap() {
            QueryKey::ItemId(b) => assert_eq!(b, [0x11u8; 32]),
            other => panic!("expected ItemId, got {other:?}"),
        }

        let key = EventsKey::ItemRevision {
            item_id: id.clone(),
            revision_id: 7,
        };
        match events_key_to_query(&key).unwrap() {
            QueryKey::ItemRevision {
                item_id,
                revision_id,
            } => {
                assert_eq!(item_id, [0x11u8; 32]);
                assert_eq!(revision_id, 7);
            }
            other => panic!("expected ItemRevision, got {other:?}"),
        }

        // Invalid hex rejects.
        let key = EventsKey::IpfsHash {
            ipfs_hash: "0xzz".into(),
        };
        assert!(events_key_to_query(&key).is_err());
    }

    #[test]
    fn coord_status_args_serialize_deserialize() {
        let args = CoordStatusArgs {};
        let json = serde_json::to_string(&args).unwrap();
        let back: CoordStatusArgs = serde_json::from_str(&json).unwrap();
        let _ = back;
    }

    #[test]
    fn coord_item_args_serialize_deserialize() {
        let args = CoordItemArgs {
            item_id: "0x1234".into(),
            revision_id: Some(3),
        };
        let json = serde_json::to_string(&args).unwrap();
        let back: CoordItemArgs = serde_json::from_str(&json).unwrap();
        assert_eq!(back.item_id, "0x1234");
        assert_eq!(back.revision_id, Some(3));
    }

    #[test]
    fn coord_publish_item_args_serialize_deserialize() {
        let args = CoordPublishItemArgs {
            account: "5Grwva".into(),
            content: ContentInput::default(),
            parents: vec![],
            links: vec!["0xab".into()],
            mentions: vec![],
            flags: None,
            nonce: Some("0xcd".into()),
        };
        let json = serde_json::to_string(&args).unwrap();
        let back: CoordPublishItemArgs = serde_json::from_str(&json).unwrap();
        assert_eq!(back.links, vec!["0xab"]);
        assert_eq!(back.nonce.as_deref(), Some("0xcd"));
    }

    #[test]
    fn parse_id_list_rejects_invalid_length() {
        assert!(parse_id_list(&["0x1234".into()]).is_err());
        assert!(parse_id_list(&["0xzz".into()]).is_err());
        assert!(parse_id_list(&[]).unwrap().is_empty());
    }
}
