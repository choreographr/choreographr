//! Substrate chain client — subxt against the Coordination Platform node.
//!
//! This is the crate's only async path: it drives `subxt` (node RPC + tx
//! submission) on the [tokio sidecar] so the daemon stays thread-only. IPFS and
//! the indexer are synchronous and never touch this runtime.
//!
//! ## Signing
//!
//! A Polkadot account is stored in the keystore as its **expanded ed25519
//! secret** (the 64 bytes that Polkadot-JS stores in its scrypt+XSalsa20
//! envelope, see `choreo-keystore::substrate`). `subxt-signer` cannot rebuild a
//! keypair from that form, so this module reconstructs a `schnorrkel::Keypair`
//! and wraps it in a [`ChoreoSigner`] implementing `subxt::tx::Signer<PolkadotConfig>`.
//!
//! [tokio sidecar]: crate::runtime

use sp_core::crypto::Ss58Codec;
use sp_core::sr25519::Public;
use subxt::OnlineClient;
use subxt::PolkadotConfig;
use subxt::config::Config;
use subxt::utils::{AccountId32, MultiAddress, MultiSignature};

use crate::CoordError;
use crate::acuity_runtime::api;
use crate::config::CHAIN_WS_URL;
use zeroize::Zeroizing;

/// A signing keypair rebuilt from a stored expanded ed25519 secret.
struct ChoreoSigner(schnorrkel::Keypair);

impl ChoreoSigner {
    /// Build a signer from a 64-byte expanded ed25519 secret key.
    fn from_expanded_secret(secret: &[u8]) -> Result<Self, CoordError> {
        let secret_key: [u8; 64] = secret
            .try_into()
            .map_err(|_| CoordError::Account("expected a 64-byte ed25519 secret".into()))?;
        let secret = schnorrkel::SecretKey::from_ed25519_bytes(&secret_key)
            .map_err(|e| CoordError::Account(format!("invalid ed25519 secret: {e}")))?;
        let public = secret.to_public();
        Ok(Self(schnorrkel::Keypair { public, secret }))
    }

    /// The raw 32-byte public key (=== account id).
    fn account_id(&self) -> [u8; 32] {
        self.0.public.to_bytes()
    }
}

impl subxt::tx::Signer<PolkadotConfig> for ChoreoSigner {
    fn account_id(&self) -> <PolkadotConfig as Config>::AccountId {
        AccountId32(self.account_id())
    }
    fn sign(&self, signer_payload: &[u8]) -> <PolkadotConfig as Config>::Signature {
        // sr25519 signs over a Schnorrkel signing context (matching subxt-signer
        // and Substrate's on-chain verification).
        let context = schnorrkel::signing_context(b"substrate");
        let sig = self.0.sign(context.bytes(signer_payload));
        MultiSignature::Sr25519(sig.to_bytes())
    }
}

/// Parse an SS58 account address into its raw 32-byte account id.
pub fn account_id_from_address(address: &str) -> Result<[u8; 32], CoordError> {
    let public = Public::from_ss58check(address)
        .map_err(|e| CoordError::InvalidArgument(format!("invalid SS58 address {address}: {e}")))?;
    Ok(public.0)
}

/// Render a raw 32-byte account id as an SS58 address (chain prefix).
pub fn account_address(bytes: [u8; 32]) -> String {
    AccountId32(bytes).to_string()
}

/// A chain account (public id + expanded secret), ready to sign & submit.
pub struct ChainAccount {
    /// SS58 address.
    pub address: String,
    /// Raw 32-byte account id.
    pub account_id: [u8; 32],
    /// Expanded ed25519 secret (64 bytes), zeroized on drop.
    ///
    /// The secret is cloned out of the `#[zeroize(drop)]` `ServiceCredential`
    /// into this per-call account, so it is held here in a `Zeroizing` buffer
    /// to ensure the private key is wiped from memory when the account is
    /// dropped (not left to the allocator).
    secret: Zeroizing<Vec<u8>>,
}

impl ChainAccount {
    /// Build a chain account from a 32-byte account id + 64-byte expanded secret.
    pub fn from_parts(account_id: [u8; 32], secret: Vec<u8>) -> Self {
        Self {
            address: account_address(account_id),
            account_id,
            secret: Zeroizing::new(secret),
        }
    }
    /// Build a chain account from an SS58 address + 64-byte expanded secret
    /// (validates that the address matches the secret's public key).
    pub fn from_address(address: &str, secret: Vec<u8>) -> Result<Self, CoordError> {
        let signer = ChoreoSigner::from_expanded_secret(&secret)?;
        let account_id = account_id_from_address(address)?;
        if signer.account_id() != account_id {
            return Err(CoordError::Account(format!(
                "address {address} does not match the supplied secret"
            )));
        }
        Ok(Self {
            address: address.to_string(),
            account_id,
            secret: Zeroizing::new(secret),
        })
    }
    fn signer(&self) -> Result<ChoreoSigner, CoordError> {
        ChoreoSigner::from_expanded_secret(&self.secret)
    }
}

/// Connect to the node (async; run on the sidecar).
///
/// Verifies the connected node is the Coordination Platform by comparing its
/// reported genesis hash to the pinned [`crate::config::GENESIS_HASH`], so a
/// different chain at the same endpoint is rejected before any call is issued.
async fn connect() -> Result<OnlineClient<PolkadotConfig>, CoordError> {
    let client = OnlineClient::<PolkadotConfig>::from_insecure_url(CHAIN_WS_URL)
        .await
        .map_err(|e| CoordError::Substrate(format!("failed to connect to {CHAIN_WS_URL}: {e}")))?;

    let actual = crate::encode::bytes_to_hex(client.genesis_hash().as_ref());
    if actual != crate::config::GENESIS_HASH {
        return Err(CoordError::Substrate(format!(
            "node genesis {actual} does not match expected {}",
            crate::config::GENESIS_HASH
        )));
    }
    Ok(client)
}

/// Per-chain-operation wall-clock budget. A hung node (or an RPC that never
/// answers) must not block a daemon tool thread indefinitely, so every subxt
/// future runs inside this timeout on the sidecar.
const CHAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Run a chain async operation, bounding it with [`CHAIN_TIMEOUT`]. The future
/// must resolve to a `CoordError`-bearing `Result` so a timeout maps to a
/// [`CoordError::Substrate`] that flows through the caller's `?`.
async fn with_chain_timeout<T, F>(fut: F) -> Result<T, CoordError>
where
    F: std::future::Future<Output = Result<T, CoordError>>,
{
    match tokio::time::timeout(CHAIN_TIMEOUT, fut).await {
        Ok(res) => res,
        Err(_) => Err(CoordError::Substrate("chain RPC timed out".into())),
    }
}

/// A finalized transaction result (what write tools report).
#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub struct TxOutcome {
    /// The item id emitted by `Content::PublishItem`, if this call created one.
    pub item_id: Option<String>,
}

/// Submit a call, wait for finalized success, and return the finalized events.
/// Shared by every write extrinsic; the publish wrappers additionally parse the
/// created item id from those events (see [`submit_publish`]).
async fn submit_and_wait<Call>(
    call: &Call,
    account: &ChainAccount,
) -> Result<subxt::extrinsics::ExtrinsicEvents<PolkadotConfig>, CoordError>
where
    Call: subxt::tx::Payload,
{
    let client = connect().await?;
    let at = client
        .at_current_block()
        .await
        .map_err(|e| CoordError::Substrate(format!("failed to get block for tx: {e}")))?;
    let signer = account.signer()?;
    at.tx()
        .sign_and_submit_then_watch_default(call, &signer)
        .await
        .map_err(|e| CoordError::Transaction(format!("submit failed: {e}")))?
        .wait_for_finalized_success()
        .await
        .map_err(|e| CoordError::Transaction(format!("finalized failed: {e}")))
}

/// Submit a publish call and capture the `Content::PublishItem` item id.
async fn submit_publish<Call>(call: &Call, account: &ChainAccount) -> Result<TxOutcome, CoordError>
where
    Call: subxt::tx::Payload,
{
    let tx_events = submit_and_wait(call, account).await?;
    let item_id = tx_events
        .find_first::<api::content::events::PublishItem>()
        .transpose()
        .map_err(|e| CoordError::Transaction(format!("failed to decode PublishItem: {e}")))?
        .map(|evt| crate::encode::bytes_to_hex(&evt.item_id.0));
    Ok(TxOutcome { item_id })
}

// ── Read (on-chain authoritative state) ──────────────────────────────────────

/// Read an item's on-chain control state (owner, revision, flags).
pub fn item_state(item_id: [u8; 32]) -> Result<ItemState, CoordError> {
    crate::runtime::block_on(with_chain_timeout(async move {
        let client = connect().await?;
        let at = client
            .at_current_block()
            .await
            .map_err(|e| CoordError::Substrate(format!("failed to get block: {e}")))?;
        let addr = api::storage().content().item_state();
        let thunk = at
            .storage()
            .try_fetch(
                addr,
                (api::runtime_types::pallet_content::pallet::ItemId(item_id),),
            )
            .await
            .map_err(|e| CoordError::Substrate(format!("item state query failed: {e}")))?;
        match thunk {
            Some(thunk) => {
                let item = thunk
                    .decode()
                    .map_err(|e| CoordError::Substrate(format!("decode item failed: {e}")))?;
                Ok(ItemState {
                    owner: crate::encode::bytes_to_hex(&item.owner.0),
                    revision_id: item.revision_id,
                    flags: item.flags,
                })
            }
            None => Err(CoordError::Content("item not found on-chain".into())),
        }
    }))?
}

/// Read the list of item ids an account has pinned in `pallet-account-content`.
pub fn account_item_ids(account: [u8; 32]) -> Result<Vec<[u8; 32]>, CoordError> {
    crate::runtime::block_on(with_chain_timeout(async move {
        let client = connect().await?;
        let at = client
            .at_current_block()
            .await
            .map_err(|e| CoordError::Substrate(format!("failed to get block: {e}")))?;
        let addr = api::storage().account_content().account_item_ids();
        let thunk = at
            .storage()
            .try_fetch(addr, (AccountId32(account),))
            .await
            .map_err(|e| CoordError::Substrate(format!("account items query failed: {e}")))?;
        match thunk {
            Some(thunk) => {
                let bounded = thunk
                    .decode()
                    .map_err(|e| CoordError::Substrate(format!("decode items failed: {e}")))?;
                Ok(bounded.0.into_iter().map(|id| id.0).collect())
            }
            None => Ok(Vec::new()),
        }
    }))?
}

/// Read the profile item id an account has set (`pallet-account-profile`).
pub fn profile_item(account: [u8; 32]) -> Result<Option<[u8; 32]>, CoordError> {
    crate::runtime::block_on(with_chain_timeout(async move {
        let client = connect().await?;
        let at = client
            .at_current_block()
            .await
            .map_err(|e| CoordError::Substrate(format!("failed to get block: {e}")))?;
        let addr = api::storage().account_profile().account_profile();
        let thunk = at
            .storage()
            .try_fetch(addr, (AccountId32(account),))
            .await
            .map_err(|e| CoordError::Substrate(format!("profile query failed: {e}")))?;
        match thunk {
            Some(thunk) => {
                let decoded = thunk
                    .decode()
                    .map_err(|e| CoordError::Substrate(format!("decode profile failed: {e}")))?;
                Ok(Some(decoded.0))
            }
            None => Ok(None),
        }
    }))?
}

/// On-chain control state of an item.
#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub struct ItemState {
    /// Owner account id (hex `0x`).
    pub owner: String,
    /// Latest revision counter.
    pub revision_id: u32,
    /// Lifecycle flag bitmask.
    pub flags: u8,
}

// ── Bounded-collection helpers ───────────────────────────────────────────────

use api::runtime_types::bounded_collections::bounded_vec::BoundedVec;
use api::runtime_types::pallet_content::pallet::ItemId;

/// Build a content `BoundedVec<ItemId>` from raw item-id bytes.
fn item_bounded(list: &[[u8; 32]]) -> BoundedVec<ItemId> {
    BoundedVec(list.iter().map(|b| ItemId(*b)).collect())
}

/// Build a `BoundedVec<AccountId32>` from raw account-id bytes.
fn account_bounded(list: &[[u8; 32]]) -> BoundedVec<AccountId32> {
    BoundedVec(list.iter().map(|b| AccountId32(*b)).collect())
}

// ── Write (submit extrinsics) ────────────────────────────────────────────────

/// Publish a brand-new item (optionally batched with an account add).
pub fn publish_item(
    account: &ChainAccount,
    nonce: [u8; 32],
    parents: Vec<[u8; 32]>,
    flags: u8,
    links: Vec<[u8; 32]>,
    mentions: Vec<[u8; 32]>,
    ipfs_hash: [u8; 32],
) -> Result<TxOutcome, CoordError> {
    let call = api::tx().content().publish_item(
        api::runtime_types::pallet_content::Nonce(nonce),
        item_bounded(&parents),
        flags,
        item_bounded(&links),
        account_bounded(&mentions),
        api::runtime_types::pallet_content::pallet::IpfsHash(ipfs_hash),
    );
    crate::runtime::block_on(with_chain_timeout(submit_publish(&call, account)))?
}

/// Publish a new revision of an existing item.
pub fn publish_revision(
    account: &ChainAccount,
    item_id: [u8; 32],
    links: Vec<[u8; 32]>,
    mentions: Vec<[u8; 32]>,
    ipfs_hash: [u8; 32],
) -> Result<TxOutcome, CoordError> {
    let call = api::tx().content().publish_revision(
        ItemId(item_id),
        item_bounded(&links),
        account_bounded(&mentions),
        api::runtime_types::pallet_content::pallet::IpfsHash(ipfs_hash),
    );
    crate::runtime::block_on(with_chain_timeout(submit_publish(&call, account)))?
}

/// Retract an item.
pub fn retract_item(account: &ChainAccount, item_id: [u8; 32]) -> Result<(), CoordError> {
    let call = api::tx()
        .content()
        .retract_item(api::runtime_types::pallet_content::pallet::ItemId(item_id));
    let _ = crate::runtime::block_on(with_chain_timeout(submit_and_wait(&call, account)))?;
    Ok(())
}

/// Clear the REVISIONABLE flag on an item.
pub fn set_not_revisionable(account: &ChainAccount, item_id: [u8; 32]) -> Result<(), CoordError> {
    let call = api::tx()
        .content()
        .set_not_revisionable(api::runtime_types::pallet_content::pallet::ItemId(item_id));
    let _ = crate::runtime::block_on(with_chain_timeout(submit_and_wait(&call, account)))?;
    Ok(())
}

/// Clear the RETRACTABLE flag on an item.
pub fn set_not_retractable(account: &ChainAccount, item_id: [u8; 32]) -> Result<(), CoordError> {
    let call = api::tx()
        .content()
        .set_not_retractable(api::runtime_types::pallet_content::pallet::ItemId(item_id));
    let _ = crate::runtime::block_on(with_chain_timeout(submit_and_wait(&call, account)))?;
    Ok(())
}

/// Pin an item to an account (`account_content::add_item`).
pub fn add_account_item(account: &ChainAccount, item_id: [u8; 32]) -> Result<(), CoordError> {
    let call = api::tx()
        .account_content()
        .add_item(api::runtime_types::pallet_content::pallet::ItemId(item_id));
    let _ = crate::runtime::block_on(with_chain_timeout(submit_and_wait(&call, account)))?;
    Ok(())
}

/// Unpin an item from an account (`account_content::remove_item`).
pub fn remove_account_item(account: &ChainAccount, item_id: [u8; 32]) -> Result<(), CoordError> {
    let call = api::tx()
        .account_content()
        .remove_item(api::runtime_types::pallet_content::pallet::ItemId(item_id));
    let _ = crate::runtime::block_on(submit_publish(&call, account))?;
    Ok(())
}

/// Point an account's profile at an item (`account_profile::set_profile`).
pub fn set_profile(account: &ChainAccount, item_id: [u8; 32]) -> Result<(), CoordError> {
    let call = api::tx()
        .account_profile()
        .set_profile(api::runtime_types::pallet_content::pallet::ItemId(item_id));
    let _ = crate::runtime::block_on(with_chain_timeout(submit_and_wait(&call, account)))?;
    Ok(())
}

/// The `MultiAddress` identity for an account (for extrinsics that need it).
#[allow(dead_code)]
pub(crate) fn multi_address(account: [u8; 32]) -> MultiAddress<AccountId32, ()> {
    MultiAddress::Id(AccountId32(account))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_id_from_address_round_trips() {
        let mut bytes = [0u8; 32];
        bytes[0] = 9;
        let address = account_address(bytes);
        assert_eq!(account_id_from_address(&address).unwrap(), bytes);
    }

    #[test]
    fn account_id_from_address_rejects_garbage() {
        assert!(account_id_from_address("not-an-address").is_err());
    }
}
