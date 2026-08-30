#[allow(
    dead_code,
    missing_docs,
    unused_imports,
    non_camel_case_types,
    unreachable_patterns
)]
#[allow(clippy::all)]
#[allow(rustdoc::broken_intra_doc_links)]
pub mod api {
    #[allow(unused_imports)]
    mod root_mod {
        pub use super::*;
    }
    pub static PALLETS: [&str; 12usize] = [
        "System",
        "Timestamp",
        "ParachainSystem",
        "Aura",
        "Balances",
        "Sudo",
        "TransactionPayment",
        "Content",
        "AccountContent",
        "AccountProfile",
        "ContentReactions",
        "Utility",
    ];
    pub static RUNTIME_APIS: [&str; 11usize] = [
        "Core",
        "Metadata",
        "BlockBuilder",
        "TaggedTransactionQueue",
        "OffchainWorkerApi",
        "SessionKeys",
        "AuraApi",
        "GetParachainInfo",
        "AccountNonceApi",
        "TransactionPaymentApi",
        "GenesisBuilder",
    ];
    #[doc = r" The error type that is returned when there is a runtime issue."]
    pub type DispatchError = runtime_types::sp_runtime::DispatchError;
    #[doc = r" The outer event enum."]
    pub type Event = runtime_types::acuity_runtime::RuntimeEvent;
    #[doc = r" The outer extrinsic enum."]
    pub type Call = runtime_types::acuity_runtime::RuntimeCall;
    #[doc = r" The outer error enum represents the DispatchError's Module variant."]
    pub type Error = runtime_types::acuity_runtime::RuntimeError;
    pub fn constants() -> ConstantsApi {
        ConstantsApi
    }
    pub fn storage() -> StorageApi {
        StorageApi
    }
    #[doc = r" This is an alias to [`Self::transactions()`]."]
    pub fn tx() -> TransactionApi {
        TransactionApi
    }
    pub fn transactions() -> TransactionApi {
        TransactionApi
    }
    pub fn runtime_apis() -> runtime_apis::RuntimeApi {
        runtime_apis::RuntimeApi
    }
    pub mod runtime_apis {
        use super::root_mod;
        use super::runtime_types;
        use ::subxt::ext::codec::Encode;
        pub struct RuntimeApi;
        impl RuntimeApi {
            pub fn core(&self) -> core::Core {
                core::Core
            }
            pub fn metadata(&self) -> metadata::Metadata {
                metadata::Metadata
            }
            pub fn block_builder(&self) -> block_builder::BlockBuilder {
                block_builder::BlockBuilder
            }
            pub fn tagged_transaction_queue(
                &self,
            ) -> tagged_transaction_queue::TaggedTransactionQueue {
                tagged_transaction_queue::TaggedTransactionQueue
            }
            pub fn offchain_worker_api(&self) -> offchain_worker_api::OffchainWorkerApi {
                offchain_worker_api::OffchainWorkerApi
            }
            pub fn session_keys(&self) -> session_keys::SessionKeys {
                session_keys::SessionKeys
            }
            pub fn aura_api(&self) -> aura_api::AuraApi {
                aura_api::AuraApi
            }
            pub fn get_parachain_info(&self) -> get_parachain_info::GetParachainInfo {
                get_parachain_info::GetParachainInfo
            }
            pub fn account_nonce_api(&self) -> account_nonce_api::AccountNonceApi {
                account_nonce_api::AccountNonceApi
            }
            pub fn transaction_payment_api(
                &self,
            ) -> transaction_payment_api::TransactionPaymentApi {
                transaction_payment_api::TransactionPaymentApi
            }
            pub fn genesis_builder(&self) -> genesis_builder::GenesisBuilder {
                genesis_builder::GenesisBuilder
            }
        }
        pub mod core {
            use super::root_mod;
            use super::runtime_types;
            #[doc = " The `Core` runtime api that every Substrate runtime needs to implement."]
            pub struct Core;
            impl Core {
                #[doc = " Returns the version of the runtime."]
                pub fn version(
                    &self,
                ) -> ::subxt::runtime_apis::StaticPayload<(), version::output::Output>
                {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "Core",
                        "version",
                        (),
                        [
                            79u8, 22u8, 137u8, 4u8, 40u8, 64u8, 30u8, 180u8, 49u8, 222u8, 114u8,
                            125u8, 44u8, 25u8, 33u8, 152u8, 98u8, 42u8, 72u8, 178u8, 240u8, 103u8,
                            34u8, 187u8, 81u8, 161u8, 183u8, 6u8, 120u8, 2u8, 146u8, 0u8,
                        ],
                    )
                }
                #[doc = " Execute the given block."]
                pub fn execute_block(
                    &self,
                    block: execute_block::Block,
                ) -> ::subxt::runtime_apis::StaticPayload<
                    (execute_block::Block,),
                    execute_block::output::Output,
                > {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "Core",
                        "execute_block",
                        (block,),
                        [
                            133u8, 135u8, 228u8, 65u8, 106u8, 27u8, 85u8, 158u8, 112u8, 254u8,
                            93u8, 26u8, 102u8, 201u8, 118u8, 216u8, 249u8, 247u8, 91u8, 74u8, 56u8,
                            208u8, 231u8, 115u8, 131u8, 29u8, 209u8, 6u8, 65u8, 57u8, 214u8, 125u8,
                        ],
                    )
                }
                #[doc = " Initialize a block with the given header and return the runtime executive mode."]
                pub fn initialize_block(
                    &self,
                    header: initialize_block::Header,
                ) -> ::subxt::runtime_apis::StaticPayload<
                    (initialize_block::Header,),
                    initialize_block::output::Output,
                > {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "Core",
                        "initialize_block",
                        (header,),
                        [
                            132u8, 169u8, 113u8, 112u8, 80u8, 139u8, 113u8, 35u8, 41u8, 81u8, 36u8,
                            35u8, 37u8, 202u8, 29u8, 207u8, 205u8, 229u8, 145u8, 7u8, 133u8, 94u8,
                            25u8, 108u8, 233u8, 86u8, 234u8, 29u8, 236u8, 57u8, 56u8, 186u8,
                        ],
                    )
                }
            }
            pub mod version {
                use super::root_mod;
                use super::runtime_types;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = runtime_types::sp_version::RuntimeVersion;
                }
            }
            pub mod execute_block {
                use super::root_mod;
                use super::runtime_types;
                pub type Block = runtime_types :: sp_runtime :: generic :: block :: LazyBlock < runtime_types :: sp_runtime :: generic :: header :: Header < :: core :: primitive :: u32 > , :: subxt :: utils :: UncheckedExtrinsic < :: subxt :: utils :: MultiAddress < :: subxt :: utils :: AccountId32 , () > , runtime_types :: acuity_runtime :: RuntimeCall , runtime_types :: sp_runtime :: MultiSignature , (runtime_types :: frame_system :: extensions :: authorize_call :: AuthorizeCall , runtime_types :: frame_system :: extensions :: check_non_zero_sender :: CheckNonZeroSender , runtime_types :: frame_system :: extensions :: check_spec_version :: CheckSpecVersion , runtime_types :: frame_system :: extensions :: check_tx_version :: CheckTxVersion , runtime_types :: frame_system :: extensions :: check_genesis :: CheckGenesis , runtime_types :: frame_system :: extensions :: check_mortality :: CheckMortality , runtime_types :: frame_system :: extensions :: check_nonce :: CheckNonce , runtime_types :: frame_system :: extensions :: check_weight :: CheckWeight , runtime_types :: pallet_transaction_payment :: ChargeTransactionPayment , runtime_types :: frame_system :: extensions :: weight_reclaim :: WeightReclaim ,) > > ;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = ();
                }
            }
            pub mod initialize_block {
                use super::root_mod;
                use super::runtime_types;
                pub type Header =
                    runtime_types::sp_runtime::generic::header::Header<::core::primitive::u32>;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = runtime_types::sp_runtime::ExtrinsicInclusionMode;
                }
            }
        }
        pub mod metadata {
            use super::root_mod;
            use super::runtime_types;
            #[doc = " The `Metadata` api trait that returns metadata for the runtime."]
            pub struct Metadata;
            impl Metadata {
                #[doc = " Returns the metadata of a runtime."]
                pub fn metadata(
                    &self,
                ) -> ::subxt::runtime_apis::StaticPayload<(), metadata::output::Output>
                {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "Metadata",
                        "metadata",
                        (),
                        [
                            231u8, 24u8, 67u8, 152u8, 23u8, 26u8, 188u8, 82u8, 229u8, 6u8, 185u8,
                            27u8, 175u8, 68u8, 83u8, 122u8, 69u8, 89u8, 185u8, 74u8, 248u8, 87u8,
                            217u8, 124u8, 193u8, 252u8, 199u8, 186u8, 196u8, 179u8, 179u8, 96u8,
                        ],
                    )
                }
                #[doc = " Returns the metadata at a given version."]
                #[doc = ""]
                #[doc = " If the given `version` isn't supported, this will return `None`."]
                #[doc = " Use [`Self::metadata_versions`] to find out about supported metadata version of the runtime."]
                pub fn metadata_at_version(
                    &self,
                    version: metadata_at_version::Version,
                ) -> ::subxt::runtime_apis::StaticPayload<
                    (metadata_at_version::Version,),
                    metadata_at_version::output::Output,
                > {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "Metadata",
                        "metadata_at_version",
                        (version,),
                        [
                            131u8, 53u8, 212u8, 234u8, 16u8, 25u8, 120u8, 252u8, 153u8, 153u8,
                            216u8, 28u8, 54u8, 113u8, 52u8, 236u8, 146u8, 68u8, 142u8, 8u8, 10u8,
                            169u8, 131u8, 142u8, 204u8, 38u8, 48u8, 108u8, 134u8, 86u8, 226u8,
                            61u8,
                        ],
                    )
                }
                #[doc = " Returns the supported metadata versions."]
                #[doc = ""]
                #[doc = " This can be used to call `metadata_at_version`."]
                pub fn metadata_versions(
                    &self,
                ) -> ::subxt::runtime_apis::StaticPayload<(), metadata_versions::output::Output>
                {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "Metadata",
                        "metadata_versions",
                        (),
                        [
                            23u8, 144u8, 137u8, 91u8, 188u8, 39u8, 231u8, 208u8, 252u8, 218u8,
                            224u8, 176u8, 77u8, 32u8, 130u8, 212u8, 223u8, 76u8, 100u8, 190u8,
                            82u8, 94u8, 190u8, 8u8, 82u8, 244u8, 225u8, 179u8, 85u8, 176u8, 56u8,
                            16u8,
                        ],
                    )
                }
            }
            pub mod metadata {
                use super::root_mod;
                use super::runtime_types;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = runtime_types::sp_core::OpaqueMetadata;
                }
            }
            pub mod metadata_at_version {
                use super::root_mod;
                use super::runtime_types;
                pub type Version = ::core::primitive::u32;
                pub mod output {
                    use super::runtime_types;
                    pub type Output =
                        ::core::option::Option<runtime_types::sp_core::OpaqueMetadata>;
                }
            }
            pub mod metadata_versions {
                use super::root_mod;
                use super::runtime_types;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = ::subxt::alloc::vec::Vec<::core::primitive::u32>;
                }
            }
        }
        pub mod block_builder {
            use super::root_mod;
            use super::runtime_types;
            #[doc = " The `BlockBuilder` api trait that provides the required functionality for building a block."]
            pub struct BlockBuilder;
            impl BlockBuilder {
                #[doc = " Apply the given extrinsic."]
                #[doc = ""]
                #[doc = " Returns an inclusion outcome which specifies if this extrinsic is included in"]
                #[doc = " this block or not."]
                pub fn apply_extrinsic(
                    &self,
                    extrinsic: apply_extrinsic::Extrinsic,
                ) -> ::subxt::runtime_apis::StaticPayload<
                    (apply_extrinsic::Extrinsic,),
                    apply_extrinsic::output::Output,
                > {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "BlockBuilder",
                        "apply_extrinsic",
                        (extrinsic,),
                        [
                            192u8, 184u8, 199u8, 4u8, 85u8, 136u8, 214u8, 205u8, 29u8, 29u8, 98u8,
                            145u8, 172u8, 92u8, 168u8, 161u8, 150u8, 133u8, 100u8, 243u8, 100u8,
                            100u8, 118u8, 28u8, 104u8, 82u8, 93u8, 63u8, 79u8, 36u8, 149u8, 144u8,
                        ],
                    )
                }
                #[doc = " Finish the current block."]
                pub fn finalize_block(
                    &self,
                ) -> ::subxt::runtime_apis::StaticPayload<(), finalize_block::output::Output>
                {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "BlockBuilder",
                        "finalize_block",
                        (),
                        [
                            244u8, 207u8, 24u8, 33u8, 13u8, 69u8, 9u8, 249u8, 145u8, 143u8, 122u8,
                            96u8, 197u8, 55u8, 64u8, 111u8, 238u8, 224u8, 34u8, 201u8, 27u8, 146u8,
                            232u8, 99u8, 191u8, 30u8, 114u8, 16u8, 32u8, 220u8, 58u8, 62u8,
                        ],
                    )
                }
                #[doc = " Generate inherent extrinsics. The inherent data will vary from chain to chain."]
                pub fn inherent_extrinsics(
                    &self,
                    inherent: inherent_extrinsics::Inherent,
                ) -> ::subxt::runtime_apis::StaticPayload<
                    (inherent_extrinsics::Inherent,),
                    inherent_extrinsics::output::Output,
                > {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "BlockBuilder",
                        "inherent_extrinsics",
                        (inherent,),
                        [
                            254u8, 110u8, 245u8, 201u8, 250u8, 192u8, 27u8, 228u8, 151u8, 213u8,
                            166u8, 89u8, 94u8, 81u8, 189u8, 234u8, 64u8, 18u8, 245u8, 80u8, 29u8,
                            18u8, 140u8, 129u8, 113u8, 236u8, 135u8, 55u8, 79u8, 159u8, 175u8,
                            183u8,
                        ],
                    )
                }
                #[doc = " Check that the inherents are valid. The inherent data will vary from chain to chain."]
                pub fn check_inherents(
                    &self,
                    block: check_inherents::Block,
                    data: check_inherents::Data,
                ) -> ::subxt::runtime_apis::StaticPayload<
                    (check_inherents::Block, check_inherents::Data),
                    check_inherents::output::Output,
                > {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "BlockBuilder",
                        "check_inherents",
                        (block, data),
                        [
                            153u8, 134u8, 1u8, 215u8, 139u8, 11u8, 53u8, 51u8, 210u8, 175u8, 197u8,
                            28u8, 38u8, 209u8, 175u8, 247u8, 142u8, 157u8, 50u8, 151u8, 164u8,
                            191u8, 181u8, 118u8, 80u8, 97u8, 160u8, 248u8, 110u8, 217u8, 181u8,
                            234u8,
                        ],
                    )
                }
            }
            pub mod apply_extrinsic {
                use super::root_mod;
                use super::runtime_types;
                pub type Extrinsic = :: subxt :: utils :: UncheckedExtrinsic < :: subxt :: utils :: MultiAddress < :: subxt :: utils :: AccountId32 , () > , runtime_types :: acuity_runtime :: RuntimeCall , runtime_types :: sp_runtime :: MultiSignature , (runtime_types :: frame_system :: extensions :: authorize_call :: AuthorizeCall , runtime_types :: frame_system :: extensions :: check_non_zero_sender :: CheckNonZeroSender , runtime_types :: frame_system :: extensions :: check_spec_version :: CheckSpecVersion , runtime_types :: frame_system :: extensions :: check_tx_version :: CheckTxVersion , runtime_types :: frame_system :: extensions :: check_genesis :: CheckGenesis , runtime_types :: frame_system :: extensions :: check_mortality :: CheckMortality , runtime_types :: frame_system :: extensions :: check_nonce :: CheckNonce , runtime_types :: frame_system :: extensions :: check_weight :: CheckWeight , runtime_types :: pallet_transaction_payment :: ChargeTransactionPayment , runtime_types :: frame_system :: extensions :: weight_reclaim :: WeightReclaim ,) > ;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = ::core::result::Result<
                        ::core::result::Result<(), runtime_types::sp_runtime::DispatchError>,
                        runtime_types::sp_runtime::transaction_validity::TransactionValidityError,
                    >;
                }
            }
            pub mod finalize_block {
                use super::root_mod;
                use super::runtime_types;
                pub mod output {
                    use super::runtime_types;
                    pub type Output =
                        runtime_types::sp_runtime::generic::header::Header<::core::primitive::u32>;
                }
            }
            pub mod inherent_extrinsics {
                use super::root_mod;
                use super::runtime_types;
                pub type Inherent = runtime_types::sp_inherents::InherentData;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = :: subxt :: alloc :: vec :: Vec < :: subxt :: utils :: UncheckedExtrinsic < :: subxt :: utils :: MultiAddress < :: subxt :: utils :: AccountId32 , () > , runtime_types :: acuity_runtime :: RuntimeCall , runtime_types :: sp_runtime :: MultiSignature , (runtime_types :: frame_system :: extensions :: authorize_call :: AuthorizeCall , runtime_types :: frame_system :: extensions :: check_non_zero_sender :: CheckNonZeroSender , runtime_types :: frame_system :: extensions :: check_spec_version :: CheckSpecVersion , runtime_types :: frame_system :: extensions :: check_tx_version :: CheckTxVersion , runtime_types :: frame_system :: extensions :: check_genesis :: CheckGenesis , runtime_types :: frame_system :: extensions :: check_mortality :: CheckMortality , runtime_types :: frame_system :: extensions :: check_nonce :: CheckNonce , runtime_types :: frame_system :: extensions :: check_weight :: CheckWeight , runtime_types :: pallet_transaction_payment :: ChargeTransactionPayment , runtime_types :: frame_system :: extensions :: weight_reclaim :: WeightReclaim ,) > > ;
                }
            }
            pub mod check_inherents {
                use super::root_mod;
                use super::runtime_types;
                pub type Block = runtime_types :: sp_runtime :: generic :: block :: LazyBlock < runtime_types :: sp_runtime :: generic :: header :: Header < :: core :: primitive :: u32 > , :: subxt :: utils :: UncheckedExtrinsic < :: subxt :: utils :: MultiAddress < :: subxt :: utils :: AccountId32 , () > , runtime_types :: acuity_runtime :: RuntimeCall , runtime_types :: sp_runtime :: MultiSignature , (runtime_types :: frame_system :: extensions :: authorize_call :: AuthorizeCall , runtime_types :: frame_system :: extensions :: check_non_zero_sender :: CheckNonZeroSender , runtime_types :: frame_system :: extensions :: check_spec_version :: CheckSpecVersion , runtime_types :: frame_system :: extensions :: check_tx_version :: CheckTxVersion , runtime_types :: frame_system :: extensions :: check_genesis :: CheckGenesis , runtime_types :: frame_system :: extensions :: check_mortality :: CheckMortality , runtime_types :: frame_system :: extensions :: check_nonce :: CheckNonce , runtime_types :: frame_system :: extensions :: check_weight :: CheckWeight , runtime_types :: pallet_transaction_payment :: ChargeTransactionPayment , runtime_types :: frame_system :: extensions :: weight_reclaim :: WeightReclaim ,) > > ;
                pub type Data = runtime_types::sp_inherents::InherentData;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = runtime_types::sp_inherents::CheckInherentsResult;
                }
            }
        }
        pub mod tagged_transaction_queue {
            use super::root_mod;
            use super::runtime_types;
            #[doc = " The `TaggedTransactionQueue` api trait for interfering with the transaction queue."]
            pub struct TaggedTransactionQueue;
            impl TaggedTransactionQueue {
                #[doc = " Validate the transaction."]
                #[doc = ""]
                #[doc = " This method is invoked by the transaction pool to learn details about given transaction."]
                #[doc = " The implementation should make sure to verify the correctness of the transaction"]
                #[doc = " against current state. The given `block_hash` corresponds to the hash of the block"]
                #[doc = " that is used as current state."]
                #[doc = ""]
                #[doc = " Note that this call may be performed by the pool multiple times and transactions"]
                #[doc = " might be verified in any possible order."]
                pub fn validate_transaction(
                    &self,
                    source: validate_transaction::Source,
                    tx: validate_transaction::Tx,
                    block_hash: validate_transaction::BlockHash,
                ) -> ::subxt::runtime_apis::StaticPayload<
                    (
                        validate_transaction::Source,
                        validate_transaction::Tx,
                        validate_transaction::BlockHash,
                    ),
                    validate_transaction::output::Output,
                > {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "TaggedTransactionQueue",
                        "validate_transaction",
                        (source, tx, block_hash),
                        [
                            19u8, 53u8, 170u8, 115u8, 75u8, 121u8, 231u8, 50u8, 199u8, 181u8,
                            243u8, 170u8, 163u8, 224u8, 213u8, 134u8, 206u8, 207u8, 88u8, 242u8,
                            80u8, 139u8, 233u8, 87u8, 175u8, 249u8, 178u8, 169u8, 255u8, 171u8,
                            4u8, 125u8,
                        ],
                    )
                }
            }
            pub mod validate_transaction {
                use super::root_mod;
                use super::runtime_types;
                pub type Source =
                    runtime_types::sp_runtime::transaction_validity::TransactionSource;
                pub type Tx = :: subxt :: utils :: UncheckedExtrinsic < :: subxt :: utils :: MultiAddress < :: subxt :: utils :: AccountId32 , () > , runtime_types :: acuity_runtime :: RuntimeCall , runtime_types :: sp_runtime :: MultiSignature , (runtime_types :: frame_system :: extensions :: authorize_call :: AuthorizeCall , runtime_types :: frame_system :: extensions :: check_non_zero_sender :: CheckNonZeroSender , runtime_types :: frame_system :: extensions :: check_spec_version :: CheckSpecVersion , runtime_types :: frame_system :: extensions :: check_tx_version :: CheckTxVersion , runtime_types :: frame_system :: extensions :: check_genesis :: CheckGenesis , runtime_types :: frame_system :: extensions :: check_mortality :: CheckMortality , runtime_types :: frame_system :: extensions :: check_nonce :: CheckNonce , runtime_types :: frame_system :: extensions :: check_weight :: CheckWeight , runtime_types :: pallet_transaction_payment :: ChargeTransactionPayment , runtime_types :: frame_system :: extensions :: weight_reclaim :: WeightReclaim ,) > ;
                pub type BlockHash = ::subxt::utils::H256;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = ::core::result::Result<
                        runtime_types::sp_runtime::transaction_validity::ValidTransaction,
                        runtime_types::sp_runtime::transaction_validity::TransactionValidityError,
                    >;
                }
            }
        }
        pub mod offchain_worker_api {
            use super::root_mod;
            use super::runtime_types;
            #[doc = " The offchain worker api."]
            pub struct OffchainWorkerApi;
            impl OffchainWorkerApi {
                #[doc = " Starts the off-chain task for given block header."]
                pub fn offchain_worker(
                    &self,
                    header: offchain_worker::Header,
                ) -> ::subxt::runtime_apis::StaticPayload<
                    (offchain_worker::Header,),
                    offchain_worker::output::Output,
                > {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "OffchainWorkerApi",
                        "offchain_worker",
                        (header,),
                        [
                            10u8, 135u8, 19u8, 153u8, 33u8, 216u8, 18u8, 242u8, 33u8, 140u8, 4u8,
                            223u8, 200u8, 130u8, 103u8, 118u8, 137u8, 24u8, 19u8, 127u8, 161u8,
                            29u8, 184u8, 111u8, 222u8, 111u8, 253u8, 73u8, 45u8, 31u8, 79u8, 60u8,
                        ],
                    )
                }
            }
            pub mod offchain_worker {
                use super::root_mod;
                use super::runtime_types;
                pub type Header =
                    runtime_types::sp_runtime::generic::header::Header<::core::primitive::u32>;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = ();
                }
            }
        }
        pub mod session_keys {
            use super::root_mod;
            use super::runtime_types;
            #[doc = " Session keys runtime api."]
            pub struct SessionKeys;
            impl SessionKeys {
                #[doc = " Generate a set of session keys with optionally using the given seed."]
                #[doc = " The keys should be stored within the keystore exposed via runtime"]
                #[doc = " externalities."]
                #[doc = ""]
                #[doc = " The seed needs to be a valid `utf8` string."]
                #[doc = ""]
                #[doc = " Returns the concatenated SCALE encoded public keys."]
                pub fn generate_session_keys(
                    &self,
                    owner: generate_session_keys::Owner,
                    seed: generate_session_keys::Seed,
                ) -> ::subxt::runtime_apis::StaticPayload<
                    (generate_session_keys::Owner, generate_session_keys::Seed),
                    generate_session_keys::output::Output,
                > {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "SessionKeys",
                        "generate_session_keys",
                        (owner, seed),
                        [
                            94u8, 230u8, 217u8, 119u8, 217u8, 37u8, 67u8, 190u8, 118u8, 204u8,
                            72u8, 95u8, 58u8, 138u8, 153u8, 164u8, 95u8, 31u8, 85u8, 83u8, 199u8,
                            12u8, 119u8, 135u8, 248u8, 96u8, 85u8, 142u8, 84u8, 238u8, 111u8,
                            254u8,
                        ],
                    )
                }
                #[doc = " Decode the given public session keys."]
                #[doc = ""]
                #[doc = " Returns the list of public raw public keys + key type."]
                pub fn decode_session_keys(
                    &self,
                    encoded: decode_session_keys::Encoded,
                ) -> ::subxt::runtime_apis::StaticPayload<
                    (decode_session_keys::Encoded,),
                    decode_session_keys::output::Output,
                > {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "SessionKeys",
                        "decode_session_keys",
                        (encoded,),
                        [
                            57u8, 242u8, 18u8, 51u8, 132u8, 110u8, 238u8, 255u8, 39u8, 194u8, 8u8,
                            54u8, 198u8, 178u8, 75u8, 151u8, 148u8, 176u8, 144u8, 197u8, 87u8,
                            29u8, 179u8, 235u8, 176u8, 78u8, 252u8, 103u8, 72u8, 203u8, 151u8,
                            248u8,
                        ],
                    )
                }
            }
            pub mod generate_session_keys {
                use super::root_mod;
                use super::runtime_types;
                pub type Owner = ::subxt::alloc::vec::Vec<::core::primitive::u8>;
                pub type Seed =
                    ::core::option::Option<::subxt::alloc::vec::Vec<::core::primitive::u8>>;
                pub mod output {
                    use super::runtime_types;
                    pub type Output =
                        runtime_types::sp_session::runtime_api::OpaqueGeneratedSessionKeys;
                }
            }
            pub mod decode_session_keys {
                use super::root_mod;
                use super::runtime_types;
                pub type Encoded = ::subxt::alloc::vec::Vec<::core::primitive::u8>;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = ::core::option::Option<
                        ::subxt::alloc::vec::Vec<(
                            ::subxt::alloc::vec::Vec<::core::primitive::u8>,
                            runtime_types::sp_core::crypto::KeyTypeId,
                        )>,
                    >;
                }
            }
        }
        pub mod aura_api {
            use super::root_mod;
            use super::runtime_types;
            #[doc = " API necessary for block authorship with aura."]
            pub struct AuraApi;
            impl AuraApi {
                #[doc = " Returns the slot duration for Aura."]
                #[doc = ""]
                #[doc = " Currently, only the value provided by this type at genesis will be used."]
                pub fn slot_duration(
                    &self,
                ) -> ::subxt::runtime_apis::StaticPayload<(), slot_duration::output::Output>
                {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "AuraApi",
                        "slot_duration",
                        (),
                        [
                            233u8, 210u8, 132u8, 172u8, 100u8, 125u8, 239u8, 92u8, 114u8, 82u8,
                            7u8, 110u8, 179u8, 196u8, 10u8, 19u8, 211u8, 15u8, 174u8, 2u8, 91u8,
                            73u8, 133u8, 100u8, 205u8, 201u8, 191u8, 60u8, 163u8, 122u8, 215u8,
                            10u8,
                        ],
                    )
                }
                #[doc = " Return the current set of authorities."]
                pub fn authorities(
                    &self,
                ) -> ::subxt::runtime_apis::StaticPayload<(), authorities::output::Output>
                {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "AuraApi",
                        "authorities",
                        (),
                        [
                            35u8, 244u8, 24u8, 155u8, 95u8, 1u8, 221u8, 159u8, 33u8, 144u8, 213u8,
                            26u8, 13u8, 21u8, 136u8, 72u8, 45u8, 47u8, 15u8, 51u8, 235u8, 10u8,
                            6u8, 219u8, 9u8, 246u8, 50u8, 252u8, 49u8, 77u8, 64u8, 182u8,
                        ],
                    )
                }
            }
            pub mod slot_duration {
                use super::root_mod;
                use super::runtime_types;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = runtime_types::sp_consensus_slots::SlotDuration;
                }
            }
            pub mod authorities {
                use super::root_mod;
                use super::runtime_types;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = ::subxt::alloc::vec::Vec<
                        runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public,
                    >;
                }
            }
        }
        pub mod get_parachain_info {
            use super::root_mod;
            use super::runtime_types;
            #[doc = " Runtime api used to access general info about a parachain runtime."]
            pub struct GetParachainInfo;
            impl GetParachainInfo {
                #[doc = " Retrieve the parachain id used for runtime."]
                pub fn parachain_id(
                    &self,
                ) -> ::subxt::runtime_apis::StaticPayload<(), parachain_id::output::Output>
                {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "GetParachainInfo",
                        "parachain_id",
                        (),
                        [
                            133u8, 200u8, 87u8, 39u8, 197u8, 166u8, 184u8, 238u8, 60u8, 133u8,
                            176u8, 139u8, 162u8, 6u8, 45u8, 152u8, 186u8, 33u8, 185u8, 175u8,
                            225u8, 15u8, 226u8, 54u8, 157u8, 126u8, 214u8, 90u8, 155u8, 34u8, 1u8,
                            208u8,
                        ],
                    )
                }
            }
            pub mod parachain_id {
                use super::root_mod;
                use super::runtime_types;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = runtime_types::polkadot_parachain_primitives::primitives::Id;
                }
            }
        }
        pub mod account_nonce_api {
            use super::root_mod;
            use super::runtime_types;
            #[doc = " The API to query account nonce."]
            pub struct AccountNonceApi;
            impl AccountNonceApi {
                #[doc = " Get current account nonce of given `AccountId`."]
                pub fn account_nonce(
                    &self,
                    account: account_nonce::Account,
                ) -> ::subxt::runtime_apis::StaticPayload<
                    (account_nonce::Account,),
                    account_nonce::output::Output,
                > {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "AccountNonceApi",
                        "account_nonce",
                        (account,),
                        [
                            231u8, 82u8, 7u8, 227u8, 131u8, 2u8, 215u8, 252u8, 173u8, 82u8, 11u8,
                            103u8, 200u8, 25u8, 114u8, 116u8, 79u8, 229u8, 152u8, 150u8, 236u8,
                            37u8, 101u8, 26u8, 220u8, 146u8, 182u8, 101u8, 73u8, 55u8, 191u8,
                            171u8,
                        ],
                    )
                }
            }
            pub mod account_nonce {
                use super::root_mod;
                use super::runtime_types;
                pub type Account = ::subxt::utils::AccountId32;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = ::core::primitive::u32;
                }
            }
        }
        pub mod transaction_payment_api {
            use super::root_mod;
            use super::runtime_types;
            pub struct TransactionPaymentApi;
            impl TransactionPaymentApi {
                pub fn query_info(
                    &self,
                    uxt: query_info::Uxt,
                    len: query_info::Len,
                ) -> ::subxt::runtime_apis::StaticPayload<
                    (query_info::Uxt, query_info::Len),
                    query_info::output::Output,
                > {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "TransactionPaymentApi",
                        "query_info",
                        (uxt, len),
                        [
                            56u8, 30u8, 174u8, 34u8, 202u8, 24u8, 177u8, 189u8, 145u8, 36u8, 1u8,
                            156u8, 98u8, 209u8, 178u8, 49u8, 198u8, 23u8, 150u8, 173u8, 35u8,
                            205u8, 147u8, 129u8, 42u8, 22u8, 69u8, 3u8, 129u8, 8u8, 196u8, 139u8,
                        ],
                    )
                }
                pub fn query_fee_details(
                    &self,
                    uxt: query_fee_details::Uxt,
                    len: query_fee_details::Len,
                ) -> ::subxt::runtime_apis::StaticPayload<
                    (query_fee_details::Uxt, query_fee_details::Len),
                    query_fee_details::output::Output,
                > {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "TransactionPaymentApi",
                        "query_fee_details",
                        (uxt, len),
                        [
                            117u8, 60u8, 137u8, 159u8, 237u8, 252u8, 216u8, 238u8, 232u8, 1u8,
                            100u8, 152u8, 26u8, 185u8, 145u8, 125u8, 68u8, 189u8, 4u8, 30u8, 125u8,
                            7u8, 196u8, 153u8, 235u8, 51u8, 219u8, 108u8, 185u8, 254u8, 100u8,
                            201u8,
                        ],
                    )
                }
                pub fn query_weight_to_fee(
                    &self,
                    weight: query_weight_to_fee::Weight,
                ) -> ::subxt::runtime_apis::StaticPayload<
                    (query_weight_to_fee::Weight,),
                    query_weight_to_fee::output::Output,
                > {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "TransactionPaymentApi",
                        "query_weight_to_fee",
                        (weight,),
                        [
                            206u8, 243u8, 189u8, 83u8, 231u8, 244u8, 247u8, 52u8, 126u8, 208u8,
                            224u8, 5u8, 163u8, 108u8, 254u8, 114u8, 214u8, 156u8, 227u8, 217u8,
                            211u8, 198u8, 121u8, 164u8, 110u8, 54u8, 181u8, 146u8, 50u8, 146u8,
                            146u8, 23u8,
                        ],
                    )
                }
                pub fn query_length_to_fee(
                    &self,
                    length: query_length_to_fee::Length,
                ) -> ::subxt::runtime_apis::StaticPayload<
                    (query_length_to_fee::Length,),
                    query_length_to_fee::output::Output,
                > {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "TransactionPaymentApi",
                        "query_length_to_fee",
                        (length,),
                        [
                            92u8, 132u8, 29u8, 119u8, 66u8, 11u8, 196u8, 224u8, 129u8, 23u8, 249u8,
                            12u8, 32u8, 28u8, 92u8, 50u8, 188u8, 101u8, 203u8, 229u8, 248u8, 216u8,
                            130u8, 150u8, 212u8, 161u8, 81u8, 254u8, 116u8, 89u8, 162u8, 48u8,
                        ],
                    )
                }
            }
            pub mod query_info {
                use super::root_mod;
                use super::runtime_types;
                pub type Uxt = :: subxt :: utils :: UncheckedExtrinsic < :: subxt :: utils :: MultiAddress < :: subxt :: utils :: AccountId32 , () > , runtime_types :: acuity_runtime :: RuntimeCall , runtime_types :: sp_runtime :: MultiSignature , (runtime_types :: frame_system :: extensions :: authorize_call :: AuthorizeCall , runtime_types :: frame_system :: extensions :: check_non_zero_sender :: CheckNonZeroSender , runtime_types :: frame_system :: extensions :: check_spec_version :: CheckSpecVersion , runtime_types :: frame_system :: extensions :: check_tx_version :: CheckTxVersion , runtime_types :: frame_system :: extensions :: check_genesis :: CheckGenesis , runtime_types :: frame_system :: extensions :: check_mortality :: CheckMortality , runtime_types :: frame_system :: extensions :: check_nonce :: CheckNonce , runtime_types :: frame_system :: extensions :: check_weight :: CheckWeight , runtime_types :: pallet_transaction_payment :: ChargeTransactionPayment , runtime_types :: frame_system :: extensions :: weight_reclaim :: WeightReclaim ,) > ;
                pub type Len = ::core::primitive::u32;
                pub mod output {
                    use super::runtime_types;
                    pub type Output =
                        runtime_types::pallet_transaction_payment::types::RuntimeDispatchInfo<
                            ::core::primitive::u128,
                            runtime_types::sp_weights::weight_v2::Weight,
                        >;
                }
            }
            pub mod query_fee_details {
                use super::root_mod;
                use super::runtime_types;
                pub type Uxt = :: subxt :: utils :: UncheckedExtrinsic < :: subxt :: utils :: MultiAddress < :: subxt :: utils :: AccountId32 , () > , runtime_types :: acuity_runtime :: RuntimeCall , runtime_types :: sp_runtime :: MultiSignature , (runtime_types :: frame_system :: extensions :: authorize_call :: AuthorizeCall , runtime_types :: frame_system :: extensions :: check_non_zero_sender :: CheckNonZeroSender , runtime_types :: frame_system :: extensions :: check_spec_version :: CheckSpecVersion , runtime_types :: frame_system :: extensions :: check_tx_version :: CheckTxVersion , runtime_types :: frame_system :: extensions :: check_genesis :: CheckGenesis , runtime_types :: frame_system :: extensions :: check_mortality :: CheckMortality , runtime_types :: frame_system :: extensions :: check_nonce :: CheckNonce , runtime_types :: frame_system :: extensions :: check_weight :: CheckWeight , runtime_types :: pallet_transaction_payment :: ChargeTransactionPayment , runtime_types :: frame_system :: extensions :: weight_reclaim :: WeightReclaim ,) > ;
                pub type Len = ::core::primitive::u32;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = runtime_types::pallet_transaction_payment::types::FeeDetails<
                        ::core::primitive::u128,
                    >;
                }
            }
            pub mod query_weight_to_fee {
                use super::root_mod;
                use super::runtime_types;
                pub type Weight = runtime_types::sp_weights::weight_v2::Weight;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = ::core::primitive::u128;
                }
            }
            pub mod query_length_to_fee {
                use super::root_mod;
                use super::runtime_types;
                pub type Length = ::core::primitive::u32;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = ::core::primitive::u128;
                }
            }
        }
        pub mod genesis_builder {
            use super::root_mod;
            use super::runtime_types;
            #[doc = " API to interact with `RuntimeGenesisConfig` for the runtime"]
            pub struct GenesisBuilder;
            impl GenesisBuilder {
                #[doc = " Build `RuntimeGenesisConfig` from a JSON blob not using any defaults and store it in the"]
                #[doc = " storage."]
                #[doc = ""]
                #[doc = " In the case of a FRAME-based runtime, this function deserializes the full"]
                #[doc = " `RuntimeGenesisConfig` from the given JSON blob and puts it into the storage. If the"]
                #[doc = " provided JSON blob is incorrect or incomplete or the deserialization fails, an error"]
                #[doc = " is returned."]
                #[doc = ""]
                #[doc = " Please note that provided JSON blob must contain all `RuntimeGenesisConfig` fields, no"]
                #[doc = " defaults will be used."]
                pub fn build_state(
                    &self,
                    json: build_state::Json,
                ) -> ::subxt::runtime_apis::StaticPayload<
                    (build_state::Json,),
                    build_state::output::Output,
                > {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "GenesisBuilder",
                        "build_state",
                        (json,),
                        [
                            203u8, 233u8, 104u8, 116u8, 111u8, 131u8, 201u8, 235u8, 117u8, 116u8,
                            140u8, 185u8, 93u8, 25u8, 155u8, 210u8, 56u8, 49u8, 23u8, 32u8, 253u8,
                            92u8, 149u8, 241u8, 85u8, 245u8, 137u8, 45u8, 209u8, 189u8, 81u8, 2u8,
                        ],
                    )
                }
                #[doc = " Returns a JSON blob representation of the built-in `RuntimeGenesisConfig` identified by"]
                #[doc = " `id`."]
                #[doc = ""]
                #[doc = " If `id` is `None` the function should return JSON blob representation of the default"]
                #[doc = " `RuntimeGenesisConfig` struct of the runtime. Implementation must provide default"]
                #[doc = " `RuntimeGenesisConfig`."]
                #[doc = ""]
                #[doc = " Otherwise function returns a JSON representation of the built-in, named"]
                #[doc = " `RuntimeGenesisConfig` preset identified by `id`, or `None` if such preset does not"]
                #[doc = " exist. Returned `Vec<u8>` contains bytes of JSON blob (patch) which comprises a list of"]
                #[doc = " (potentially nested) key-value pairs that are intended for customizing the default"]
                #[doc = " runtime genesis config. The patch shall be merged (rfc7386) with the JSON representation"]
                #[doc = " of the default `RuntimeGenesisConfig` to create a comprehensive genesis config that can"]
                #[doc = " be used in `build_state` method."]
                pub fn get_preset(
                    &self,
                    id: get_preset::Id,
                ) -> ::subxt::runtime_apis::StaticPayload<
                    (get_preset::Id,),
                    get_preset::output::Output,
                > {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "GenesisBuilder",
                        "get_preset",
                        (id,),
                        [
                            43u8, 153u8, 23u8, 52u8, 113u8, 161u8, 227u8, 122u8, 169u8, 135u8,
                            119u8, 8u8, 128u8, 33u8, 143u8, 235u8, 13u8, 173u8, 58u8, 121u8, 178u8,
                            223u8, 66u8, 217u8, 22u8, 244u8, 168u8, 113u8, 202u8, 186u8, 241u8,
                            124u8,
                        ],
                    )
                }
                #[doc = " Returns a list of identifiers for available builtin `RuntimeGenesisConfig` presets."]
                #[doc = ""]
                #[doc = " The presets from the list can be queried with [`GenesisBuilder::get_preset`] method. If"]
                #[doc = " no named presets are provided by the runtime the list is empty."]
                pub fn preset_names(
                    &self,
                ) -> ::subxt::runtime_apis::StaticPayload<(), preset_names::output::Output>
                {
                    ::subxt::runtime_apis::StaticPayload::new_static(
                        "GenesisBuilder",
                        "preset_names",
                        (),
                        [
                            150u8, 117u8, 54u8, 129u8, 221u8, 130u8, 186u8, 71u8, 13u8, 140u8,
                            77u8, 180u8, 141u8, 37u8, 22u8, 219u8, 149u8, 218u8, 186u8, 206u8,
                            80u8, 42u8, 165u8, 41u8, 99u8, 184u8, 73u8, 37u8, 125u8, 188u8, 167u8,
                            122u8,
                        ],
                    )
                }
            }
            pub mod build_state {
                use super::root_mod;
                use super::runtime_types;
                pub type Json = ::subxt::alloc::vec::Vec<::core::primitive::u8>;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = ::core::result::Result<(), ::subxt::alloc::string::String>;
                }
            }
            pub mod get_preset {
                use super::root_mod;
                use super::runtime_types;
                pub type Id = ::core::option::Option<::subxt::alloc::string::String>;
                pub mod output {
                    use super::runtime_types;
                    pub type Output =
                        ::core::option::Option<::subxt::alloc::vec::Vec<::core::primitive::u8>>;
                }
            }
            pub mod preset_names {
                use super::root_mod;
                use super::runtime_types;
                pub mod output {
                    use super::runtime_types;
                    pub type Output = ::subxt::alloc::vec::Vec<::subxt::alloc::string::String>;
                }
            }
        }
    }
    pub fn view_functions() -> ViewFunctionsApi {
        ViewFunctionsApi
    }
    pub fn custom_values() -> CustomValuesApi {
        CustomValuesApi
    }
    pub struct CustomValuesApi;
    impl CustomValuesApi {}
    pub struct ConstantsApi;
    impl ConstantsApi {
        pub fn system(&self) -> system::constants::ConstantsApi {
            system::constants::ConstantsApi
        }
        pub fn timestamp(&self) -> timestamp::constants::ConstantsApi {
            timestamp::constants::ConstantsApi
        }
        pub fn aura(&self) -> aura::constants::ConstantsApi {
            aura::constants::ConstantsApi
        }
        pub fn balances(&self) -> balances::constants::ConstantsApi {
            balances::constants::ConstantsApi
        }
        pub fn transaction_payment(&self) -> transaction_payment::constants::ConstantsApi {
            transaction_payment::constants::ConstantsApi
        }
        pub fn utility(&self) -> utility::constants::ConstantsApi {
            utility::constants::ConstantsApi
        }
    }
    pub struct StorageApi;
    impl StorageApi {
        pub fn system(&self) -> system::storage::StorageApi {
            system::storage::StorageApi
        }
        pub fn timestamp(&self) -> timestamp::storage::StorageApi {
            timestamp::storage::StorageApi
        }
        pub fn parachain_system(&self) -> parachain_system::storage::StorageApi {
            parachain_system::storage::StorageApi
        }
        pub fn aura(&self) -> aura::storage::StorageApi {
            aura::storage::StorageApi
        }
        pub fn balances(&self) -> balances::storage::StorageApi {
            balances::storage::StorageApi
        }
        pub fn sudo(&self) -> sudo::storage::StorageApi {
            sudo::storage::StorageApi
        }
        pub fn transaction_payment(&self) -> transaction_payment::storage::StorageApi {
            transaction_payment::storage::StorageApi
        }
        pub fn content(&self) -> content::storage::StorageApi {
            content::storage::StorageApi
        }
        pub fn account_content(&self) -> account_content::storage::StorageApi {
            account_content::storage::StorageApi
        }
        pub fn account_profile(&self) -> account_profile::storage::StorageApi {
            account_profile::storage::StorageApi
        }
    }
    pub struct TransactionApi;
    impl TransactionApi {
        pub fn system(&self) -> system::calls::api::TransactionApi {
            system::calls::api::TransactionApi
        }
        pub fn timestamp(&self) -> timestamp::calls::api::TransactionApi {
            timestamp::calls::api::TransactionApi
        }
        pub fn parachain_system(&self) -> parachain_system::calls::api::TransactionApi {
            parachain_system::calls::api::TransactionApi
        }
        pub fn balances(&self) -> balances::calls::api::TransactionApi {
            balances::calls::api::TransactionApi
        }
        pub fn sudo(&self) -> sudo::calls::api::TransactionApi {
            sudo::calls::api::TransactionApi
        }
        pub fn content(&self) -> content::calls::api::TransactionApi {
            content::calls::api::TransactionApi
        }
        pub fn account_content(&self) -> account_content::calls::api::TransactionApi {
            account_content::calls::api::TransactionApi
        }
        pub fn account_profile(&self) -> account_profile::calls::api::TransactionApi {
            account_profile::calls::api::TransactionApi
        }
        pub fn content_reactions(&self) -> content_reactions::calls::api::TransactionApi {
            content_reactions::calls::api::TransactionApi
        }
        pub fn utility(&self) -> utility::calls::api::TransactionApi {
            utility::calls::api::TransactionApi
        }
    }
    pub struct ViewFunctionsApi;
    impl ViewFunctionsApi {}
    #[doc = r" check whether the metadata provided is aligned with this statically generated code."]
    pub fn is_codegen_valid_for(metadata: &::subxt::Metadata) -> bool {
        let runtime_metadata_hash = metadata
            .hasher()
            .only_these_pallets(&PALLETS)
            .only_these_runtime_apis(&RUNTIME_APIS)
            .hash();
        runtime_metadata_hash
            == [
                237u8, 53u8, 225u8, 193u8, 28u8, 164u8, 224u8, 4u8, 217u8, 0u8, 108u8, 166u8, 45u8,
                137u8, 70u8, 153u8, 230u8, 31u8, 70u8, 122u8, 32u8, 216u8, 113u8, 114u8, 105u8,
                147u8, 164u8, 8u8, 121u8, 136u8, 162u8, 35u8,
            ]
    }
    pub mod system {
        use super::root_mod;
        use super::runtime_types;
        #[doc = "Error for the System pallet"]
        pub type Error = runtime_types::frame_system::pallet::Error;
        #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
        pub type Call = runtime_types::frame_system::pallet::Call;
        pub mod calls {
            use super::root_mod;
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Make some on-chain remark."]
            #[doc = ""]
            #[doc = "Can be executed by every `origin`."]
            pub struct Remark {
                pub remark: remark::Remark,
            }
            pub mod remark {
                use super::runtime_types;
                pub type Remark = ::subxt::alloc::vec::Vec<::core::primitive::u8>;
            }
            impl Remark {
                const PALLET_NAME: &'static str = "System";
                const CALL_NAME: &'static str = "remark";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for Remark {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Set the number of pages in the WebAssembly environment's heap."]
            pub struct SetHeapPages {
                pub pages: set_heap_pages::Pages,
            }
            pub mod set_heap_pages {
                use super::runtime_types;
                pub type Pages = ::core::primitive::u64;
            }
            impl SetHeapPages {
                const PALLET_NAME: &'static str = "System";
                const CALL_NAME: &'static str = "set_heap_pages";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for SetHeapPages {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Set the new runtime code."]
            pub struct SetCode {
                pub code: set_code::Code,
            }
            pub mod set_code {
                use super::runtime_types;
                pub type Code = ::subxt::alloc::vec::Vec<::core::primitive::u8>;
            }
            impl SetCode {
                const PALLET_NAME: &'static str = "System";
                const CALL_NAME: &'static str = "set_code";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for SetCode {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Set the new runtime code without doing any checks of the given `code`."]
            #[doc = ""]
            #[doc = "Note that runtime upgrades will not run if this is called with a not-increasing spec"]
            #[doc = "version!"]
            pub struct SetCodeWithoutChecks {
                pub code: set_code_without_checks::Code,
            }
            pub mod set_code_without_checks {
                use super::runtime_types;
                pub type Code = ::subxt::alloc::vec::Vec<::core::primitive::u8>;
            }
            impl SetCodeWithoutChecks {
                const PALLET_NAME: &'static str = "System";
                const CALL_NAME: &'static str = "set_code_without_checks";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for SetCodeWithoutChecks {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Set some items of storage."]
            pub struct SetStorage {
                pub items: set_storage::Items,
            }
            pub mod set_storage {
                use super::runtime_types;
                pub type Items = ::subxt::alloc::vec::Vec<(
                    ::subxt::alloc::vec::Vec<::core::primitive::u8>,
                    ::subxt::alloc::vec::Vec<::core::primitive::u8>,
                )>;
            }
            impl SetStorage {
                const PALLET_NAME: &'static str = "System";
                const CALL_NAME: &'static str = "set_storage";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for SetStorage {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Kill some items from storage."]
            pub struct KillStorage {
                pub keys: kill_storage::Keys,
            }
            pub mod kill_storage {
                use super::runtime_types;
                pub type Keys =
                    ::subxt::alloc::vec::Vec<::subxt::alloc::vec::Vec<::core::primitive::u8>>;
            }
            impl KillStorage {
                const PALLET_NAME: &'static str = "System";
                const CALL_NAME: &'static str = "kill_storage";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for KillStorage {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Kill all storage items with a key that starts with the given prefix."]
            #[doc = ""]
            #[doc = "**NOTE:** We rely on the Root origin to provide us the number of subkeys under"]
            #[doc = "the prefix we are removing to accurately calculate the weight of this function."]
            pub struct KillPrefix {
                pub prefix: kill_prefix::Prefix,
                pub subkeys: kill_prefix::Subkeys,
            }
            pub mod kill_prefix {
                use super::runtime_types;
                pub type Prefix = ::subxt::alloc::vec::Vec<::core::primitive::u8>;
                pub type Subkeys = ::core::primitive::u32;
            }
            impl KillPrefix {
                const PALLET_NAME: &'static str = "System";
                const CALL_NAME: &'static str = "kill_prefix";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for KillPrefix {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Make some on-chain remark and emit event."]
            pub struct RemarkWithEvent {
                pub remark: remark_with_event::Remark,
            }
            pub mod remark_with_event {
                use super::runtime_types;
                pub type Remark = ::subxt::alloc::vec::Vec<::core::primitive::u8>;
            }
            impl RemarkWithEvent {
                const PALLET_NAME: &'static str = "System";
                const CALL_NAME: &'static str = "remark_with_event";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for RemarkWithEvent {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Authorize an upgrade to a given `code_hash` for the runtime. The runtime can be supplied"]
            #[doc = "later."]
            #[doc = ""]
            #[doc = "This call requires Root origin."]
            pub struct AuthorizeUpgrade {
                pub code_hash: authorize_upgrade::CodeHash,
            }
            pub mod authorize_upgrade {
                use super::runtime_types;
                pub type CodeHash = ::subxt::utils::H256;
            }
            impl AuthorizeUpgrade {
                const PALLET_NAME: &'static str = "System";
                const CALL_NAME: &'static str = "authorize_upgrade";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for AuthorizeUpgrade {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Authorize an upgrade to a given `code_hash` for the runtime. The runtime can be supplied"]
            #[doc = "later."]
            #[doc = ""]
            #[doc = "WARNING: This authorizes an upgrade that will take place without any safety checks, for"]
            #[doc = "example that the spec name remains the same and that the version number increases. Not"]
            #[doc = "recommended for normal use. Use `authorize_upgrade` instead."]
            #[doc = ""]
            #[doc = "This call requires Root origin."]
            pub struct AuthorizeUpgradeWithoutChecks {
                pub code_hash: authorize_upgrade_without_checks::CodeHash,
            }
            pub mod authorize_upgrade_without_checks {
                use super::runtime_types;
                pub type CodeHash = ::subxt::utils::H256;
            }
            impl AuthorizeUpgradeWithoutChecks {
                const PALLET_NAME: &'static str = "System";
                const CALL_NAME: &'static str = "authorize_upgrade_without_checks";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for AuthorizeUpgradeWithoutChecks {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Provide the preimage (runtime binary) `code` for an upgrade that has been authorized."]
            #[doc = ""]
            #[doc = "If the authorization required a version check, this call will ensure the spec name"]
            #[doc = "remains unchanged and that the spec version has increased."]
            #[doc = ""]
            #[doc = "Depending on the runtime's `OnSetCode` configuration, this function may directly apply"]
            #[doc = "the new `code` in the same block or attempt to schedule the upgrade."]
            #[doc = ""]
            #[doc = "All origins are allowed."]
            pub struct ApplyAuthorizedUpgrade {
                pub code: apply_authorized_upgrade::Code,
            }
            pub mod apply_authorized_upgrade {
                use super::runtime_types;
                pub type Code = ::subxt::alloc::vec::Vec<::core::primitive::u8>;
            }
            impl ApplyAuthorizedUpgrade {
                const PALLET_NAME: &'static str = "System";
                const CALL_NAME: &'static str = "apply_authorized_upgrade";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for ApplyAuthorizedUpgrade {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            pub mod api {
                pub struct TransactionApi;
                impl TransactionApi {
                    #[doc = "Make some on-chain remark."]
                    #[doc = ""]
                    #[doc = "Can be executed by every `origin`."]
                    pub fn remark(
                        &self,
                        remark: super::remark::Remark,
                    ) -> ::subxt::transactions::StaticPayload<super::Remark> {
                        ::subxt::transactions::StaticPayload::new_static(
                            "System",
                            "remark",
                            super::Remark { remark },
                            [
                                43u8, 126u8, 180u8, 174u8, 141u8, 48u8, 52u8, 125u8, 166u8, 212u8,
                                216u8, 98u8, 100u8, 24u8, 132u8, 71u8, 101u8, 64u8, 246u8, 169u8,
                                33u8, 250u8, 147u8, 208u8, 2u8, 40u8, 129u8, 209u8, 232u8, 207u8,
                                207u8, 13u8,
                            ],
                        )
                    }
                    #[doc = "Set the number of pages in the WebAssembly environment's heap."]
                    pub fn set_heap_pages(
                        &self,
                        pages: super::set_heap_pages::Pages,
                    ) -> ::subxt::transactions::StaticPayload<super::SetHeapPages>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "System",
                            "set_heap_pages",
                            super::SetHeapPages { pages },
                            [
                                188u8, 191u8, 99u8, 216u8, 219u8, 109u8, 141u8, 50u8, 78u8, 235u8,
                                215u8, 242u8, 195u8, 24u8, 111u8, 76u8, 229u8, 64u8, 99u8, 225u8,
                                134u8, 121u8, 81u8, 209u8, 127u8, 223u8, 98u8, 215u8, 150u8, 70u8,
                                57u8, 147u8,
                            ],
                        )
                    }
                    #[doc = "Set the new runtime code."]
                    pub fn set_code(
                        &self,
                        code: super::set_code::Code,
                    ) -> ::subxt::transactions::StaticPayload<super::SetCode> {
                        ::subxt::transactions::StaticPayload::new_static(
                            "System",
                            "set_code",
                            super::SetCode { code },
                            [
                                233u8, 248u8, 88u8, 245u8, 28u8, 65u8, 25u8, 169u8, 35u8, 237u8,
                                19u8, 203u8, 136u8, 160u8, 18u8, 3u8, 20u8, 197u8, 81u8, 169u8,
                                244u8, 188u8, 27u8, 147u8, 147u8, 236u8, 65u8, 25u8, 3u8, 143u8,
                                182u8, 22u8,
                            ],
                        )
                    }
                    #[doc = "Set the new runtime code without doing any checks of the given `code`."]
                    #[doc = ""]
                    #[doc = "Note that runtime upgrades will not run if this is called with a not-increasing spec"]
                    #[doc = "version!"]
                    pub fn set_code_without_checks(
                        &self,
                        code: super::set_code_without_checks::Code,
                    ) -> ::subxt::transactions::StaticPayload<super::SetCodeWithoutChecks>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "System",
                            "set_code_without_checks",
                            super::SetCodeWithoutChecks { code },
                            [
                                82u8, 212u8, 157u8, 44u8, 70u8, 0u8, 143u8, 15u8, 109u8, 109u8,
                                107u8, 157u8, 141u8, 42u8, 169u8, 11u8, 15u8, 186u8, 252u8, 138u8,
                                10u8, 147u8, 15u8, 178u8, 247u8, 229u8, 213u8, 98u8, 207u8, 231u8,
                                119u8, 115u8,
                            ],
                        )
                    }
                    #[doc = "Set some items of storage."]
                    pub fn set_storage(
                        &self,
                        items: super::set_storage::Items,
                    ) -> ::subxt::transactions::StaticPayload<super::SetStorage>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "System",
                            "set_storage",
                            super::SetStorage { items },
                            [
                                141u8, 216u8, 52u8, 222u8, 223u8, 136u8, 123u8, 181u8, 19u8, 75u8,
                                163u8, 102u8, 229u8, 189u8, 158u8, 142u8, 95u8, 235u8, 240u8, 49u8,
                                150u8, 76u8, 78u8, 137u8, 126u8, 88u8, 183u8, 88u8, 231u8, 146u8,
                                234u8, 43u8,
                            ],
                        )
                    }
                    #[doc = "Kill some items from storage."]
                    pub fn kill_storage(
                        &self,
                        keys: super::kill_storage::Keys,
                    ) -> ::subxt::transactions::StaticPayload<super::KillStorage>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "System",
                            "kill_storage",
                            super::KillStorage { keys },
                            [
                                73u8, 63u8, 196u8, 36u8, 144u8, 114u8, 34u8, 213u8, 108u8, 93u8,
                                209u8, 234u8, 153u8, 185u8, 33u8, 91u8, 187u8, 195u8, 223u8, 130u8,
                                58u8, 156u8, 63u8, 47u8, 228u8, 249u8, 216u8, 139u8, 143u8, 177u8,
                                41u8, 35u8,
                            ],
                        )
                    }
                    #[doc = "Kill all storage items with a key that starts with the given prefix."]
                    #[doc = ""]
                    #[doc = "**NOTE:** We rely on the Root origin to provide us the number of subkeys under"]
                    #[doc = "the prefix we are removing to accurately calculate the weight of this function."]
                    pub fn kill_prefix(
                        &self,
                        prefix: super::kill_prefix::Prefix,
                        subkeys: super::kill_prefix::Subkeys,
                    ) -> ::subxt::transactions::StaticPayload<super::KillPrefix>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "System",
                            "kill_prefix",
                            super::KillPrefix { prefix, subkeys },
                            [
                                184u8, 57u8, 139u8, 24u8, 208u8, 87u8, 108u8, 215u8, 198u8, 189u8,
                                175u8, 242u8, 167u8, 215u8, 97u8, 63u8, 110u8, 166u8, 238u8, 98u8,
                                67u8, 236u8, 111u8, 110u8, 234u8, 81u8, 102u8, 5u8, 182u8, 5u8,
                                214u8, 85u8,
                            ],
                        )
                    }
                    #[doc = "Make some on-chain remark and emit event."]
                    pub fn remark_with_event(
                        &self,
                        remark: super::remark_with_event::Remark,
                    ) -> ::subxt::transactions::StaticPayload<super::RemarkWithEvent>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "System",
                            "remark_with_event",
                            super::RemarkWithEvent { remark },
                            [
                                120u8, 120u8, 153u8, 92u8, 184u8, 85u8, 34u8, 2u8, 174u8, 206u8,
                                105u8, 228u8, 233u8, 130u8, 80u8, 246u8, 228u8, 59u8, 234u8, 240u8,
                                4u8, 49u8, 147u8, 170u8, 115u8, 91u8, 149u8, 200u8, 228u8, 181u8,
                                8u8, 154u8,
                            ],
                        )
                    }
                    #[doc = "Authorize an upgrade to a given `code_hash` for the runtime. The runtime can be supplied"]
                    #[doc = "later."]
                    #[doc = ""]
                    #[doc = "This call requires Root origin."]
                    pub fn authorize_upgrade(
                        &self,
                        code_hash: super::authorize_upgrade::CodeHash,
                    ) -> ::subxt::transactions::StaticPayload<super::AuthorizeUpgrade>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "System",
                            "authorize_upgrade",
                            super::AuthorizeUpgrade { code_hash },
                            [
                                4u8, 14u8, 76u8, 107u8, 209u8, 129u8, 9u8, 39u8, 193u8, 17u8, 84u8,
                                254u8, 170u8, 214u8, 24u8, 155u8, 29u8, 184u8, 249u8, 241u8, 109u8,
                                58u8, 145u8, 131u8, 109u8, 63u8, 38u8, 165u8, 107u8, 215u8, 217u8,
                                172u8,
                            ],
                        )
                    }
                    #[doc = "Authorize an upgrade to a given `code_hash` for the runtime. The runtime can be supplied"]
                    #[doc = "later."]
                    #[doc = ""]
                    #[doc = "WARNING: This authorizes an upgrade that will take place without any safety checks, for"]
                    #[doc = "example that the spec name remains the same and that the version number increases. Not"]
                    #[doc = "recommended for normal use. Use `authorize_upgrade` instead."]
                    #[doc = ""]
                    #[doc = "This call requires Root origin."]
                    pub fn authorize_upgrade_without_checks(
                        &self,
                        code_hash: super::authorize_upgrade_without_checks::CodeHash,
                    ) -> ::subxt::transactions::StaticPayload<super::AuthorizeUpgradeWithoutChecks>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "System",
                            "authorize_upgrade_without_checks",
                            super::AuthorizeUpgradeWithoutChecks { code_hash },
                            [
                                126u8, 126u8, 55u8, 26u8, 47u8, 55u8, 66u8, 8u8, 167u8, 18u8, 29u8,
                                136u8, 146u8, 14u8, 189u8, 117u8, 16u8, 227u8, 162u8, 61u8, 149u8,
                                197u8, 104u8, 184u8, 185u8, 161u8, 99u8, 154u8, 80u8, 125u8, 181u8,
                                233u8,
                            ],
                        )
                    }
                    #[doc = "Provide the preimage (runtime binary) `code` for an upgrade that has been authorized."]
                    #[doc = ""]
                    #[doc = "If the authorization required a version check, this call will ensure the spec name"]
                    #[doc = "remains unchanged and that the spec version has increased."]
                    #[doc = ""]
                    #[doc = "Depending on the runtime's `OnSetCode` configuration, this function may directly apply"]
                    #[doc = "the new `code` in the same block or attempt to schedule the upgrade."]
                    #[doc = ""]
                    #[doc = "All origins are allowed."]
                    pub fn apply_authorized_upgrade(
                        &self,
                        code: super::apply_authorized_upgrade::Code,
                    ) -> ::subxt::transactions::StaticPayload<super::ApplyAuthorizedUpgrade>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "System",
                            "apply_authorized_upgrade",
                            super::ApplyAuthorizedUpgrade { code },
                            [
                                232u8, 107u8, 127u8, 38u8, 230u8, 29u8, 97u8, 4u8, 160u8, 191u8,
                                222u8, 156u8, 245u8, 102u8, 196u8, 141u8, 44u8, 163u8, 98u8, 68u8,
                                125u8, 32u8, 124u8, 101u8, 108u8, 93u8, 211u8, 52u8, 0u8, 231u8,
                                33u8, 227u8,
                            ],
                        )
                    }
                }
            }
        }
        #[doc = "Event for the System pallet."]
        pub type Event = runtime_types::frame_system::pallet::Event;
        pub mod events {
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "An extrinsic completed successfully."]
            pub struct ExtrinsicSuccess {
                pub dispatch_info: extrinsic_success::DispatchInfo,
            }
            pub mod extrinsic_success {
                use super::runtime_types;
                pub type DispatchInfo = runtime_types::frame_system::DispatchEventInfo;
            }
            impl ExtrinsicSuccess {
                const PALLET_NAME: &'static str = "System";
                const EVENT_NAME: &'static str = "ExtrinsicSuccess";
            }
            impl ::subxt::events::DecodeAsEvent for ExtrinsicSuccess {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "An extrinsic failed."]
            pub struct ExtrinsicFailed {
                pub dispatch_error: extrinsic_failed::DispatchError,
                pub dispatch_info: extrinsic_failed::DispatchInfo,
            }
            pub mod extrinsic_failed {
                use super::runtime_types;
                pub type DispatchError = runtime_types::sp_runtime::DispatchError;
                pub type DispatchInfo = runtime_types::frame_system::DispatchEventInfo;
            }
            impl ExtrinsicFailed {
                const PALLET_NAME: &'static str = "System";
                const EVENT_NAME: &'static str = "ExtrinsicFailed";
            }
            impl ::subxt::events::DecodeAsEvent for ExtrinsicFailed {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "`:code` was updated to the code with the given hash."]
            pub struct CodeUpdated {
                pub hash: code_updated::Hash,
            }
            pub mod code_updated {
                use super::runtime_types;
                pub type Hash = ::subxt::utils::H256;
            }
            impl CodeUpdated {
                const PALLET_NAME: &'static str = "System";
                const EVENT_NAME: &'static str = "CodeUpdated";
            }
            impl ::subxt::events::DecodeAsEvent for CodeUpdated {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "A new account was created."]
            pub struct NewAccount {
                pub account: new_account::Account,
            }
            pub mod new_account {
                use super::runtime_types;
                pub type Account = ::subxt::utils::AccountId32;
            }
            impl NewAccount {
                const PALLET_NAME: &'static str = "System";
                const EVENT_NAME: &'static str = "NewAccount";
            }
            impl ::subxt::events::DecodeAsEvent for NewAccount {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "An account was reaped."]
            pub struct KilledAccount {
                pub account: killed_account::Account,
            }
            pub mod killed_account {
                use super::runtime_types;
                pub type Account = ::subxt::utils::AccountId32;
            }
            impl KilledAccount {
                const PALLET_NAME: &'static str = "System";
                const EVENT_NAME: &'static str = "KilledAccount";
            }
            impl ::subxt::events::DecodeAsEvent for KilledAccount {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "On on-chain remark happened."]
            pub struct Remarked {
                pub sender: remarked::Sender,
                pub hash: remarked::Hash,
            }
            pub mod remarked {
                use super::runtime_types;
                pub type Sender = ::subxt::utils::AccountId32;
                pub type Hash = ::subxt::utils::H256;
            }
            impl Remarked {
                const PALLET_NAME: &'static str = "System";
                const EVENT_NAME: &'static str = "Remarked";
            }
            impl ::subxt::events::DecodeAsEvent for Remarked {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "An upgrade was authorized."]
            pub struct UpgradeAuthorized {
                pub code_hash: upgrade_authorized::CodeHash,
                pub check_version: upgrade_authorized::CheckVersion,
            }
            pub mod upgrade_authorized {
                use super::runtime_types;
                pub type CodeHash = ::subxt::utils::H256;
                pub type CheckVersion = ::core::primitive::bool;
            }
            impl UpgradeAuthorized {
                const PALLET_NAME: &'static str = "System";
                const EVENT_NAME: &'static str = "UpgradeAuthorized";
            }
            impl ::subxt::events::DecodeAsEvent for UpgradeAuthorized {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "An invalid authorized upgrade was rejected while trying to apply it."]
            pub struct RejectedInvalidAuthorizedUpgrade {
                pub code_hash: rejected_invalid_authorized_upgrade::CodeHash,
                pub error: rejected_invalid_authorized_upgrade::Error,
            }
            pub mod rejected_invalid_authorized_upgrade {
                use super::runtime_types;
                pub type CodeHash = ::subxt::utils::H256;
                pub type Error = runtime_types::sp_runtime::DispatchError;
            }
            impl RejectedInvalidAuthorizedUpgrade {
                const PALLET_NAME: &'static str = "System";
                const EVENT_NAME: &'static str = "RejectedInvalidAuthorizedUpgrade";
            }
            impl ::subxt::events::DecodeAsEvent for RejectedInvalidAuthorizedUpgrade {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
        }
        pub mod storage {
            use super::root_mod;
            use super::runtime_types;
            pub struct StorageApi;
            impl StorageApi {
                #[doc = " The full account information for a particular account ID."]
                pub fn account(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (account::input::Param0,),
                    account::Output,
                    ::subxt::utils::Maybe,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "Account",
                        [
                            181u8, 49u8, 172u8, 169u8, 233u8, 186u8, 227u8, 180u8, 188u8, 130u8,
                            4u8, 70u8, 12u8, 226u8, 233u8, 72u8, 145u8, 59u8, 210u8, 78u8, 48u8,
                            177u8, 203u8, 27u8, 216u8, 196u8, 244u8, 208u8, 26u8, 34u8, 13u8, 50u8,
                        ],
                    )
                }
                #[doc = " Total extrinsics count for the current block."]
                pub fn extrinsic_count(
                    &self,
                ) -> ::subxt::storage::StaticAddress<(), extrinsic_count::Output, ::subxt::utils::Yes>
                {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "ExtrinsicCount",
                        [
                            217u8, 77u8, 146u8, 117u8, 157u8, 10u8, 137u8, 158u8, 27u8, 206u8,
                            129u8, 195u8, 192u8, 141u8, 178u8, 6u8, 39u8, 199u8, 156u8, 101u8,
                            60u8, 4u8, 166u8, 244u8, 193u8, 255u8, 148u8, 199u8, 83u8, 157u8, 67u8,
                            193u8,
                        ],
                    )
                }
                #[doc = " Whether all inherents have been applied."]
                pub fn inherents_applied(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (),
                    inherents_applied::Output,
                    ::subxt::utils::Yes,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "InherentsApplied",
                        [
                            18u8, 210u8, 88u8, 91u8, 207u8, 11u8, 44u8, 234u8, 226u8, 71u8, 52u8,
                            99u8, 125u8, 73u8, 149u8, 37u8, 57u8, 70u8, 39u8, 156u8, 159u8, 16u8,
                            174u8, 10u8, 101u8, 172u8, 44u8, 61u8, 160u8, 139u8, 148u8, 113u8,
                        ],
                    )
                }
                #[doc = " The current weight for the block."]
                pub fn block_weight(
                    &self,
                ) -> ::subxt::storage::StaticAddress<(), block_weight::Output, ::subxt::utils::Yes>
                {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "BlockWeight",
                        [
                            30u8, 69u8, 207u8, 199u8, 27u8, 245u8, 128u8, 231u8, 49u8, 94u8, 194u8,
                            254u8, 18u8, 97u8, 20u8, 94u8, 12u8, 245u8, 93u8, 39u8, 34u8, 216u8,
                            49u8, 39u8, 128u8, 139u8, 230u8, 83u8, 10u8, 42u8, 195u8, 115u8,
                        ],
                    )
                }
                #[doc = " Total size (in bytes) of the current block."]
                #[doc = ""]
                #[doc = " Tracks the size of the header and all extrinsics."]
                pub fn block_size(
                    &self,
                ) -> ::subxt::storage::StaticAddress<(), block_size::Output, ::subxt::utils::Yes>
                {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "BlockSize",
                        [
                            189u8, 209u8, 204u8, 16u8, 123u8, 182u8, 74u8, 254u8, 0u8, 137u8,
                            184u8, 170u8, 94u8, 208u8, 251u8, 174u8, 105u8, 91u8, 184u8, 127u8,
                            194u8, 201u8, 191u8, 81u8, 121u8, 136u8, 121u8, 127u8, 4u8, 40u8,
                            179u8, 159u8,
                        ],
                    )
                }
                #[doc = " Map of block numbers to block hashes."]
                pub fn block_hash(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (block_hash::input::Param0,),
                    block_hash::Output,
                    ::subxt::utils::Maybe,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "BlockHash",
                        [
                            251u8, 175u8, 179u8, 11u8, 47u8, 25u8, 43u8, 165u8, 168u8, 224u8, 35u8,
                            57u8, 49u8, 93u8, 29u8, 47u8, 145u8, 113u8, 84u8, 200u8, 186u8, 21u8,
                            22u8, 102u8, 126u8, 233u8, 10u8, 131u8, 47u8, 32u8, 165u8, 194u8,
                        ],
                    )
                }
                #[doc = " Extrinsics data for the current block (maps an extrinsic's index to its data)."]
                pub fn extrinsic_data(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (extrinsic_data::input::Param0,),
                    extrinsic_data::Output,
                    ::subxt::utils::Maybe,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "ExtrinsicData",
                        [
                            223u8, 197u8, 229u8, 38u8, 179u8, 46u8, 153u8, 107u8, 35u8, 131u8,
                            71u8, 231u8, 213u8, 198u8, 141u8, 55u8, 2u8, 75u8, 114u8, 159u8, 0u8,
                            16u8, 128u8, 190u8, 177u8, 92u8, 225u8, 213u8, 48u8, 167u8, 29u8,
                            121u8,
                        ],
                    )
                }
                #[doc = " The current block number being processed. Set by `execute_block`."]
                pub fn number(
                    &self,
                ) -> ::subxt::storage::StaticAddress<(), number::Output, ::subxt::utils::Yes>
                {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "Number",
                        [
                            93u8, 185u8, 195u8, 173u8, 19u8, 1u8, 39u8, 245u8, 243u8, 67u8, 228u8,
                            232u8, 25u8, 15u8, 14u8, 109u8, 225u8, 34u8, 17u8, 110u8, 25u8, 154u8,
                            149u8, 46u8, 184u8, 208u8, 79u8, 254u8, 166u8, 168u8, 33u8, 173u8,
                        ],
                    )
                }
                #[doc = " Hash of the previous block."]
                pub fn parent_hash(
                    &self,
                ) -> ::subxt::storage::StaticAddress<(), parent_hash::Output, ::subxt::utils::Yes>
                {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "ParentHash",
                        [
                            252u8, 127u8, 135u8, 108u8, 14u8, 75u8, 71u8, 121u8, 36u8, 231u8, 44u8,
                            64u8, 49u8, 246u8, 24u8, 49u8, 202u8, 229u8, 242u8, 74u8, 206u8, 65u8,
                            78u8, 207u8, 12u8, 118u8, 33u8, 42u8, 130u8, 233u8, 33u8, 136u8,
                        ],
                    )
                }
                #[doc = " Digest of the current block, also part of the block header."]
                pub fn digest(
                    &self,
                ) -> ::subxt::storage::StaticAddress<(), digest::Output, ::subxt::utils::Yes>
                {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "Digest",
                        [
                            137u8, 44u8, 198u8, 131u8, 117u8, 17u8, 114u8, 93u8, 213u8, 123u8,
                            212u8, 55u8, 43u8, 34u8, 114u8, 86u8, 39u8, 18u8, 189u8, 157u8, 27u8,
                            157u8, 155u8, 159u8, 147u8, 41u8, 138u8, 195u8, 20u8, 204u8, 110u8,
                            53u8,
                        ],
                    )
                }
                #[doc = " Events deposited for the current block."]
                #[doc = ""]
                #[doc = " NOTE: The item is unbound and should therefore never be read on chain."]
                #[doc = " It could otherwise inflate the PoV size of a block."]
                #[doc = ""]
                #[doc = " Events have a large in-memory size. Box the events to not go out-of-memory"]
                #[doc = " just in case someone still reads them from within the runtime."]
                pub fn events(
                    &self,
                ) -> ::subxt::storage::StaticAddress<(), events::Output, ::subxt::utils::Yes>
                {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "Events",
                        [
                            136u8, 237u8, 210u8, 168u8, 100u8, 127u8, 122u8, 26u8, 33u8, 177u8,
                            118u8, 69u8, 157u8, 113u8, 42u8, 87u8, 104u8, 49u8, 177u8, 21u8, 155u8,
                            216u8, 148u8, 40u8, 32u8, 146u8, 170u8, 29u8, 176u8, 227u8, 88u8,
                            120u8,
                        ],
                    )
                }
                #[doc = " The number of events in the `Events<T>` list."]
                pub fn event_count(
                    &self,
                ) -> ::subxt::storage::StaticAddress<(), event_count::Output, ::subxt::utils::Yes>
                {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "EventCount",
                        [
                            32u8, 54u8, 196u8, 23u8, 224u8, 204u8, 158u8, 79u8, 151u8, 46u8, 107u8,
                            24u8, 120u8, 90u8, 137u8, 234u8, 64u8, 92u8, 174u8, 25u8, 152u8, 22u8,
                            24u8, 245u8, 243u8, 212u8, 148u8, 149u8, 122u8, 171u8, 92u8, 140u8,
                        ],
                    )
                }
                #[doc = " Mapping between a topic (represented by T::Hash) and a vector of indexes"]
                #[doc = " of events in the `<Events<T>>` list."]
                #[doc = ""]
                #[doc = " All topic vectors have deterministic storage locations depending on the topic. This"]
                #[doc = " allows light-clients to leverage the changes trie storage tracking mechanism and"]
                #[doc = " in case of changes fetch the list of events of interest."]
                #[doc = ""]
                #[doc = " The value has the type `(BlockNumberFor<T>, EventIndex)` because if we used only just"]
                #[doc = " the `EventIndex` then in case if the topic has the same contents on the next block"]
                #[doc = " no notification will be triggered thus the event might be lost."]
                pub fn event_topics(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (event_topics::input::Param0,),
                    event_topics::Output,
                    ::subxt::utils::Maybe,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "EventTopics",
                        [
                            91u8, 29u8, 70u8, 62u8, 102u8, 127u8, 50u8, 42u8, 122u8, 136u8, 211u8,
                            187u8, 165u8, 1u8, 82u8, 213u8, 58u8, 154u8, 239u8, 26u8, 213u8, 120u8,
                            8u8, 179u8, 2u8, 134u8, 90u8, 241u8, 163u8, 199u8, 98u8, 94u8,
                        ],
                    )
                }
                #[doc = " Stores the `spec_version` and `spec_name` of when the last runtime upgrade happened."]
                pub fn last_runtime_upgrade(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (),
                    last_runtime_upgrade::Output,
                    ::subxt::utils::Yes,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "LastRuntimeUpgrade",
                        [
                            239u8, 183u8, 167u8, 75u8, 149u8, 166u8, 239u8, 31u8, 200u8, 140u8,
                            61u8, 169u8, 227u8, 186u8, 101u8, 14u8, 78u8, 101u8, 19u8, 86u8, 128u8,
                            203u8, 250u8, 97u8, 210u8, 179u8, 96u8, 191u8, 226u8, 225u8, 32u8,
                            212u8,
                        ],
                    )
                }
                #[doc = " Number of blocks till the pending code upgrade is applied."]
                pub fn blocks_till_upgrade(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (),
                    blocks_till_upgrade::Output,
                    ::subxt::utils::Yes,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "BlocksTillUpgrade",
                        [
                            5u8, 76u8, 239u8, 121u8, 109u8, 31u8, 178u8, 175u8, 48u8, 175u8, 25u8,
                            101u8, 201u8, 220u8, 19u8, 118u8, 173u8, 45u8, 211u8, 17u8, 50u8,
                            134u8, 168u8, 8u8, 69u8, 29u8, 245u8, 207u8, 146u8, 204u8, 15u8, 145u8,
                        ],
                    )
                }
                #[doc = " True if we have upgraded so that `type RefCount` is `u32`. False (default) if not."]
                pub fn upgraded_to_u32_ref_count(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (),
                    upgraded_to_u32_ref_count::Output,
                    ::subxt::utils::Yes,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "UpgradedToU32RefCount",
                        [
                            121u8, 56u8, 110u8, 113u8, 59u8, 171u8, 213u8, 125u8, 149u8, 111u8,
                            171u8, 66u8, 48u8, 0u8, 129u8, 158u8, 118u8, 33u8, 255u8, 236u8, 109u8,
                            47u8, 123u8, 153u8, 40u8, 25u8, 16u8, 60u8, 248u8, 5u8, 94u8, 235u8,
                        ],
                    )
                }
                #[doc = " True if we have upgraded so that AccountInfo contains three types of `RefCount`. False"]
                #[doc = " (default) if not."]
                pub fn upgraded_to_triple_ref_count(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (),
                    upgraded_to_triple_ref_count::Output,
                    ::subxt::utils::Yes,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "UpgradedToTripleRefCount",
                        [
                            21u8, 68u8, 180u8, 14u8, 122u8, 62u8, 230u8, 35u8, 163u8, 50u8, 98u8,
                            110u8, 27u8, 46u8, 205u8, 112u8, 29u8, 175u8, 250u8, 160u8, 76u8,
                            139u8, 10u8, 64u8, 158u8, 114u8, 176u8, 180u8, 252u8, 66u8, 6u8, 103u8,
                        ],
                    )
                }
                #[doc = " The execution phase of the block."]
                pub fn execution_phase(
                    &self,
                ) -> ::subxt::storage::StaticAddress<(), execution_phase::Output, ::subxt::utils::Yes>
                {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "ExecutionPhase",
                        [
                            73u8, 148u8, 64u8, 200u8, 68u8, 224u8, 56u8, 2u8, 224u8, 156u8, 197u8,
                            124u8, 8u8, 173u8, 3u8, 36u8, 146u8, 33u8, 219u8, 205u8, 36u8, 89u8,
                            99u8, 231u8, 208u8, 2u8, 236u8, 254u8, 254u8, 108u8, 65u8, 68u8,
                        ],
                    )
                }
                #[doc = " `Some` if a code upgrade has been authorized."]
                pub fn authorized_upgrade(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (),
                    authorized_upgrade::Output,
                    ::subxt::utils::Yes,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "AuthorizedUpgrade",
                        [
                            227u8, 212u8, 35u8, 221u8, 172u8, 87u8, 76u8, 244u8, 15u8, 213u8, 25u8,
                            209u8, 213u8, 3u8, 230u8, 224u8, 81u8, 7u8, 62u8, 238u8, 51u8, 158u8,
                            221u8, 35u8, 1u8, 5u8, 213u8, 142u8, 140u8, 206u8, 216u8, 214u8,
                        ],
                    )
                }
                #[doc = " The weight reclaimed for the extrinsic."]
                #[doc = ""]
                #[doc = " This information is available until the end of the extrinsic execution."]
                #[doc = " More precisely this information is removed in `note_applied_extrinsic`."]
                #[doc = ""]
                #[doc = " Logic doing some post dispatch weight reduction must update this storage to avoid duplicate"]
                #[doc = " reduction."]
                pub fn extrinsic_weight_reclaimed(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (),
                    extrinsic_weight_reclaimed::Output,
                    ::subxt::utils::Yes,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "System",
                        "ExtrinsicWeightReclaimed",
                        [
                            205u8, 30u8, 170u8, 39u8, 212u8, 71u8, 90u8, 173u8, 142u8, 127u8,
                            164u8, 223u8, 113u8, 224u8, 161u8, 109u8, 102u8, 241u8, 4u8, 225u8,
                            105u8, 163u8, 161u8, 96u8, 69u8, 178u8, 77u8, 154u8, 222u8, 83u8,
                            106u8, 175u8,
                        ],
                    )
                }
            }
            pub mod account {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                    pub type Param0 = ::subxt::utils::AccountId32;
                }
                pub type Output = runtime_types::frame_system::AccountInfo<
                    ::core::primitive::u32,
                    runtime_types::pallet_balances::types::AccountData<::core::primitive::u128>,
                >;
            }
            pub mod extrinsic_count {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = ::core::primitive::u32;
            }
            pub mod inherents_applied {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = ::core::primitive::bool;
            }
            pub mod block_weight {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = runtime_types::frame_support::dispatch::PerDispatchClass<
                    runtime_types::sp_weights::weight_v2::Weight,
                >;
            }
            pub mod block_size {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = ::core::primitive::u32;
            }
            pub mod block_hash {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                    pub type Param0 = ::core::primitive::u32;
                }
                pub type Output = ::subxt::utils::H256;
            }
            pub mod extrinsic_data {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                    pub type Param0 = ::core::primitive::u32;
                }
                pub type Output = ::subxt::alloc::vec::Vec<::core::primitive::u8>;
            }
            pub mod number {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = ::core::primitive::u32;
            }
            pub mod parent_hash {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = ::subxt::utils::H256;
            }
            pub mod digest {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = runtime_types::sp_runtime::generic::digest::Digest;
            }
            pub mod events {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = ::subxt::alloc::vec::Vec<
                    runtime_types::frame_system::EventRecord<
                        runtime_types::acuity_runtime::RuntimeEvent,
                        ::subxt::utils::H256,
                    >,
                >;
            }
            pub mod event_count {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = ::core::primitive::u32;
            }
            pub mod event_topics {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                    pub type Param0 = ::subxt::utils::H256;
                }
                pub type Output =
                    ::subxt::alloc::vec::Vec<(::core::primitive::u32, ::core::primitive::u32)>;
            }
            pub mod last_runtime_upgrade {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = runtime_types::frame_system::LastRuntimeUpgradeInfo;
            }
            pub mod blocks_till_upgrade {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = ::core::primitive::u8;
            }
            pub mod upgraded_to_u32_ref_count {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = ::core::primitive::bool;
            }
            pub mod upgraded_to_triple_ref_count {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = ::core::primitive::bool;
            }
            pub mod execution_phase {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = runtime_types::frame_system::Phase;
            }
            pub mod authorized_upgrade {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = runtime_types::frame_system::CodeUpgradeAuthorization;
            }
            pub mod extrinsic_weight_reclaimed {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = runtime_types::sp_weights::weight_v2::Weight;
            }
        }
        pub mod constants {
            use super::runtime_types;
            pub struct ConstantsApi;
            impl ConstantsApi {
                #[doc = " Block & extrinsics weights: base values and limits."]
                pub fn block_weights(
                    &self,
                ) -> ::subxt::constants::StaticAddress<
                    runtime_types::frame_system::limits::BlockWeights,
                > {
                    ::subxt::constants::StaticAddress::new_static(
                        "System",
                        "BlockWeights",
                        [
                            176u8, 124u8, 225u8, 136u8, 25u8, 73u8, 247u8, 33u8, 82u8, 206u8, 85u8,
                            190u8, 127u8, 102u8, 71u8, 11u8, 185u8, 8u8, 58u8, 0u8, 94u8, 55u8,
                            163u8, 177u8, 104u8, 59u8, 60u8, 136u8, 246u8, 116u8, 0u8, 239u8,
                        ],
                    )
                }
                #[doc = " The maximum length of a block (in bytes)."]
                pub fn block_length(
                    &self,
                ) -> ::subxt::constants::StaticAddress<
                    runtime_types::frame_system::limits::BlockLength,
                > {
                    ::subxt::constants::StaticAddress::new_static(
                        "System",
                        "BlockLength",
                        [
                            25u8, 97u8, 176u8, 77u8, 2u8, 60u8, 44u8, 69u8, 161u8, 69u8, 251u8,
                            229u8, 198u8, 186u8, 185u8, 237u8, 105u8, 56u8, 122u8, 35u8, 78u8,
                            195u8, 98u8, 222u8, 215u8, 49u8, 249u8, 146u8, 231u8, 21u8, 224u8,
                            134u8,
                        ],
                    )
                }
                #[doc = " Maximum number of block number to block hash mappings to keep (oldest pruned first)."]
                pub fn block_hash_count(
                    &self,
                ) -> ::subxt::constants::StaticAddress<::core::primitive::u32> {
                    ::subxt::constants::StaticAddress::new_static(
                        "System",
                        "BlockHashCount",
                        [
                            98u8, 252u8, 116u8, 72u8, 26u8, 180u8, 225u8, 83u8, 200u8, 157u8,
                            125u8, 151u8, 53u8, 76u8, 168u8, 26u8, 10u8, 9u8, 98u8, 68u8, 9u8,
                            178u8, 197u8, 113u8, 31u8, 79u8, 200u8, 90u8, 203u8, 100u8, 41u8,
                            145u8,
                        ],
                    )
                }
                #[doc = " The weight of runtime database operations the runtime can invoke."]
                pub fn db_weight(
                    &self,
                ) -> ::subxt::constants::StaticAddress<runtime_types::sp_weights::RuntimeDbWeight>
                {
                    ::subxt::constants::StaticAddress::new_static(
                        "System",
                        "DbWeight",
                        [
                            42u8, 43u8, 178u8, 142u8, 243u8, 203u8, 60u8, 173u8, 118u8, 111u8,
                            200u8, 170u8, 102u8, 70u8, 237u8, 187u8, 198u8, 120u8, 153u8, 232u8,
                            183u8, 76u8, 74u8, 10u8, 70u8, 243u8, 14u8, 218u8, 213u8, 126u8, 29u8,
                            177u8,
                        ],
                    )
                }
                #[doc = " Get the chain's in-code version."]
                pub fn version(
                    &self,
                ) -> ::subxt::constants::StaticAddress<runtime_types::sp_version::RuntimeVersion>
                {
                    ::subxt::constants::StaticAddress::new_static(
                        "System",
                        "Version",
                        [
                            214u8, 43u8, 96u8, 193u8, 96u8, 213u8, 63u8, 124u8, 22u8, 111u8, 41u8,
                            78u8, 146u8, 77u8, 34u8, 163u8, 117u8, 100u8, 6u8, 216u8, 238u8, 54u8,
                            80u8, 185u8, 219u8, 11u8, 192u8, 200u8, 129u8, 88u8, 161u8, 250u8,
                        ],
                    )
                }
                #[doc = " The designated SS58 prefix of this chain."]
                #[doc = ""]
                #[doc = " This replaces the \"ss58Format\" property declared in the chain spec. Reason is"]
                #[doc = " that the runtime should know about the prefix in order to make use of it as"]
                #[doc = " an identifier of the chain."]
                pub fn ss58_prefix(
                    &self,
                ) -> ::subxt::constants::StaticAddress<::core::primitive::u16> {
                    ::subxt::constants::StaticAddress::new_static(
                        "System",
                        "SS58Prefix",
                        [
                            116u8, 33u8, 2u8, 170u8, 181u8, 147u8, 171u8, 169u8, 167u8, 227u8,
                            41u8, 144u8, 11u8, 236u8, 82u8, 100u8, 74u8, 60u8, 184u8, 72u8, 169u8,
                            90u8, 208u8, 135u8, 15u8, 117u8, 10u8, 123u8, 128u8, 193u8, 29u8, 70u8,
                        ],
                    )
                }
            }
        }
    }
    pub mod timestamp {
        use super::root_mod;
        use super::runtime_types;
        #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
        pub type Call = runtime_types::pallet_timestamp::pallet::Call;
        pub mod calls {
            use super::root_mod;
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Set the current time."]
            #[doc = ""]
            #[doc = "This call should be invoked exactly once per block. It will panic at the finalization"]
            #[doc = "phase, if this call hasn't been invoked by that time."]
            #[doc = ""]
            #[doc = "The timestamp should be greater than the previous one by the amount specified by"]
            #[doc = "[`Config::MinimumPeriod`]."]
            #[doc = ""]
            #[doc = "The dispatch origin for this call must be _None_."]
            #[doc = ""]
            #[doc = "This dispatch class is _Mandatory_ to ensure it gets executed in the block. Be aware"]
            #[doc = "that changing the complexity of this call could result exhausting the resources in a"]
            #[doc = "block to execute any other calls."]
            #[doc = ""]
            #[doc = "## Complexity"]
            #[doc = "- `O(1)` (Note that implementations of `OnTimestampSet` must also be `O(1)`)"]
            #[doc = "- 1 storage read and 1 storage mutation (codec `O(1)` because of `DidUpdate::take` in"]
            #[doc = "  `on_finalize`)"]
            #[doc = "- 1 event handler `on_timestamp_set`. Must be `O(1)`."]
            pub struct Set {
                #[codec(compact)]
                pub now: set::Now,
            }
            pub mod set {
                use super::runtime_types;
                pub type Now = ::core::primitive::u64;
            }
            impl Set {
                const PALLET_NAME: &'static str = "Timestamp";
                const CALL_NAME: &'static str = "set";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for Set {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            pub mod api {
                pub struct TransactionApi;
                impl TransactionApi {
                    #[doc = "Set the current time."]
                    #[doc = ""]
                    #[doc = "This call should be invoked exactly once per block. It will panic at the finalization"]
                    #[doc = "phase, if this call hasn't been invoked by that time."]
                    #[doc = ""]
                    #[doc = "The timestamp should be greater than the previous one by the amount specified by"]
                    #[doc = "[`Config::MinimumPeriod`]."]
                    #[doc = ""]
                    #[doc = "The dispatch origin for this call must be _None_."]
                    #[doc = ""]
                    #[doc = "This dispatch class is _Mandatory_ to ensure it gets executed in the block. Be aware"]
                    #[doc = "that changing the complexity of this call could result exhausting the resources in a"]
                    #[doc = "block to execute any other calls."]
                    #[doc = ""]
                    #[doc = "## Complexity"]
                    #[doc = "- `O(1)` (Note that implementations of `OnTimestampSet` must also be `O(1)`)"]
                    #[doc = "- 1 storage read and 1 storage mutation (codec `O(1)` because of `DidUpdate::take` in"]
                    #[doc = "  `on_finalize`)"]
                    #[doc = "- 1 event handler `on_timestamp_set`. Must be `O(1)`."]
                    pub fn set(
                        &self,
                        now: super::set::Now,
                    ) -> ::subxt::transactions::StaticPayload<super::Set> {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Timestamp",
                            "set",
                            super::Set { now },
                            [
                                37u8, 95u8, 49u8, 218u8, 24u8, 22u8, 0u8, 95u8, 72u8, 35u8, 155u8,
                                199u8, 213u8, 54u8, 207u8, 22u8, 185u8, 193u8, 221u8, 70u8, 18u8,
                                200u8, 4u8, 231u8, 195u8, 173u8, 6u8, 122u8, 11u8, 203u8, 231u8,
                                227u8,
                            ],
                        )
                    }
                }
            }
        }
        pub mod storage {
            use super::root_mod;
            use super::runtime_types;
            pub struct StorageApi;
            impl StorageApi {
                #[doc = " The current time for the current block."]
                pub fn now(
                    &self,
                ) -> ::subxt::storage::StaticAddress<(), now::Output, ::subxt::utils::Yes>
                {
                    ::subxt::storage::StaticAddress::new_static(
                        "Timestamp",
                        "Now",
                        [
                            33u8, 56u8, 170u8, 83u8, 141u8, 145u8, 85u8, 240u8, 128u8, 31u8, 207u8,
                            119u8, 3u8, 202u8, 67u8, 6u8, 117u8, 189u8, 75u8, 35u8, 142u8, 183u8,
                            127u8, 182u8, 208u8, 169u8, 153u8, 229u8, 251u8, 53u8, 181u8, 45u8,
                        ],
                    )
                }
                #[doc = " Whether the timestamp has been updated in this block."]
                #[doc = ""]
                #[doc = " This value is updated to `true` upon successful submission of a timestamp by a node."]
                #[doc = " It is then checked at the end of each block execution in the `on_finalize` hook."]
                pub fn did_update(
                    &self,
                ) -> ::subxt::storage::StaticAddress<(), did_update::Output, ::subxt::utils::Yes>
                {
                    ::subxt::storage::StaticAddress::new_static(
                        "Timestamp",
                        "DidUpdate",
                        [
                            159u8, 174u8, 212u8, 192u8, 172u8, 1u8, 246u8, 2u8, 150u8, 55u8, 251u8,
                            62u8, 194u8, 210u8, 15u8, 214u8, 177u8, 160u8, 15u8, 138u8, 142u8,
                            125u8, 113u8, 227u8, 201u8, 250u8, 223u8, 252u8, 123u8, 144u8, 209u8,
                            10u8,
                        ],
                    )
                }
            }
            pub mod now {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = ::core::primitive::u64;
            }
            pub mod did_update {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = ::core::primitive::bool;
            }
        }
        pub mod constants {
            use super::runtime_types;
            pub struct ConstantsApi;
            impl ConstantsApi {
                #[doc = " The minimum period between blocks."]
                #[doc = ""]
                #[doc = " Be aware that this is different to the *expected* period that the block production"]
                #[doc = " apparatus provides. Your chosen consensus system will generally work with this to"]
                #[doc = " determine a sensible block time. For example, in the Aura pallet it will be double this"]
                #[doc = " period on default settings."]
                pub fn minimum_period(
                    &self,
                ) -> ::subxt::constants::StaticAddress<::core::primitive::u64> {
                    ::subxt::constants::StaticAddress::new_static(
                        "Timestamp",
                        "MinimumPeriod",
                        [
                            128u8, 214u8, 205u8, 242u8, 181u8, 142u8, 124u8, 231u8, 190u8, 146u8,
                            59u8, 226u8, 157u8, 101u8, 103u8, 117u8, 249u8, 65u8, 18u8, 191u8,
                            103u8, 119u8, 53u8, 85u8, 81u8, 96u8, 220u8, 42u8, 184u8, 239u8, 42u8,
                            246u8,
                        ],
                    )
                }
            }
        }
    }
    pub mod parachain_system {
        use super::root_mod;
        use super::runtime_types;
        #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
        pub type Call = runtime_types::staging_parachain_info::pallet::Call;
        pub mod calls {
            use super::root_mod;
            use super::runtime_types;
            pub mod api {
                pub struct TransactionApi;
                impl TransactionApi {}
            }
        }
        pub mod storage {
            use super::root_mod;
            use super::runtime_types;
            pub struct StorageApi;
            impl StorageApi {
                pub fn parachain_id(
                    &self,
                ) -> ::subxt::storage::StaticAddress<(), parachain_id::Output, ::subxt::utils::Yes>
                {
                    ::subxt::storage::StaticAddress::new_static(
                        "ParachainSystem",
                        "ParachainId",
                        [
                            139u8, 6u8, 228u8, 14u8, 72u8, 167u8, 10u8, 200u8, 65u8, 24u8, 199u8,
                            158u8, 72u8, 243u8, 228u8, 95u8, 151u8, 77u8, 15u8, 245u8, 186u8, 23u8,
                            180u8, 223u8, 101u8, 250u8, 41u8, 85u8, 156u8, 1u8, 65u8, 55u8,
                        ],
                    )
                }
            }
            pub mod parachain_id {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = runtime_types::polkadot_parachain_primitives::primitives::Id;
            }
        }
    }
    pub mod aura {
        use super::root_mod;
        use super::runtime_types;
        pub mod storage {
            use super::root_mod;
            use super::runtime_types;
            pub struct StorageApi;
            impl StorageApi {
                #[doc = " The current authority set."]
                pub fn authorities(
                    &self,
                ) -> ::subxt::storage::StaticAddress<(), authorities::Output, ::subxt::utils::Yes>
                {
                    ::subxt::storage::StaticAddress::new_static(
                        "Aura",
                        "Authorities",
                        [
                            115u8, 164u8, 253u8, 15u8, 110u8, 193u8, 198u8, 238u8, 70u8, 39u8,
                            107u8, 5u8, 197u8, 103u8, 95u8, 110u8, 83u8, 156u8, 209u8, 81u8, 44u8,
                            44u8, 8u8, 12u8, 0u8, 98u8, 33u8, 100u8, 228u8, 128u8, 8u8, 88u8,
                        ],
                    )
                }
                #[doc = " The current slot of this block."]
                #[doc = ""]
                #[doc = " This will be set in `on_initialize`."]
                pub fn current_slot(
                    &self,
                ) -> ::subxt::storage::StaticAddress<(), current_slot::Output, ::subxt::utils::Yes>
                {
                    ::subxt::storage::StaticAddress::new_static(
                        "Aura",
                        "CurrentSlot",
                        [
                            43u8, 143u8, 102u8, 240u8, 243u8, 39u8, 191u8, 181u8, 112u8, 100u8,
                            100u8, 92u8, 169u8, 252u8, 192u8, 187u8, 231u8, 43u8, 235u8, 136u8,
                            116u8, 180u8, 82u8, 36u8, 140u8, 92u8, 203u8, 143u8, 4u8, 90u8, 86u8,
                            13u8,
                        ],
                    )
                }
            }
            pub mod authorities {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = runtime_types::bounded_collections::bounded_vec::BoundedVec<
                    runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public,
                >;
            }
            pub mod current_slot {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = runtime_types::sp_consensus_slots::Slot;
            }
        }
        pub mod constants {
            use super::runtime_types;
            pub struct ConstantsApi;
            impl ConstantsApi {
                #[doc = " The slot duration Aura should run with, expressed in milliseconds."]
                #[doc = ""]
                #[doc = " The effective value of this type can be changed with a runtime upgrade."]
                #[doc = ""]
                #[doc = " For backwards compatibility either use [`MinimumPeriodTimesTwo`] or a const."]
                pub fn slot_duration(
                    &self,
                ) -> ::subxt::constants::StaticAddress<::core::primitive::u64> {
                    ::subxt::constants::StaticAddress::new_static(
                        "Aura",
                        "SlotDuration",
                        [
                            128u8, 214u8, 205u8, 242u8, 181u8, 142u8, 124u8, 231u8, 190u8, 146u8,
                            59u8, 226u8, 157u8, 101u8, 103u8, 117u8, 249u8, 65u8, 18u8, 191u8,
                            103u8, 119u8, 53u8, 85u8, 81u8, 96u8, 220u8, 42u8, 184u8, 239u8, 42u8,
                            246u8,
                        ],
                    )
                }
            }
        }
    }
    pub mod balances {
        use super::root_mod;
        use super::runtime_types;
        #[doc = "The `Error` enum of this pallet."]
        pub type Error = runtime_types::pallet_balances::pallet::Error;
        #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
        pub type Call = runtime_types::pallet_balances::pallet::Call;
        pub mod calls {
            use super::root_mod;
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Transfer some liquid free balance to another account."]
            #[doc = ""]
            #[doc = "`transfer_allow_death` will set the `FreeBalance` of the sender and receiver."]
            #[doc = "If the sender's account is below the existential deposit as a result"]
            #[doc = "of the transfer, the account will be reaped."]
            #[doc = ""]
            #[doc = "The dispatch origin for this call must be `Signed` by the transactor."]
            pub struct TransferAllowDeath {
                pub dest: transfer_allow_death::Dest,
                #[codec(compact)]
                pub value: transfer_allow_death::Value,
            }
            pub mod transfer_allow_death {
                use super::runtime_types;
                pub type Dest = ::subxt::utils::MultiAddress<::subxt::utils::AccountId32, ()>;
                pub type Value = ::core::primitive::u128;
            }
            impl TransferAllowDeath {
                const PALLET_NAME: &'static str = "Balances";
                const CALL_NAME: &'static str = "transfer_allow_death";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for TransferAllowDeath {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Exactly as `transfer_allow_death`, except the origin must be root and the source account"]
            #[doc = "may be specified."]
            pub struct ForceTransfer {
                pub source: force_transfer::Source,
                pub dest: force_transfer::Dest,
                #[codec(compact)]
                pub value: force_transfer::Value,
            }
            pub mod force_transfer {
                use super::runtime_types;
                pub type Source = ::subxt::utils::MultiAddress<::subxt::utils::AccountId32, ()>;
                pub type Dest = ::subxt::utils::MultiAddress<::subxt::utils::AccountId32, ()>;
                pub type Value = ::core::primitive::u128;
            }
            impl ForceTransfer {
                const PALLET_NAME: &'static str = "Balances";
                const CALL_NAME: &'static str = "force_transfer";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for ForceTransfer {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Same as the [`transfer_allow_death`] call, but with a check that the transfer will not"]
            #[doc = "kill the origin account."]
            #[doc = ""]
            #[doc = "99% of the time you want [`transfer_allow_death`] instead."]
            #[doc = ""]
            #[doc = "[`transfer_allow_death`]: struct.Pallet.html#method.transfer"]
            pub struct TransferKeepAlive {
                pub dest: transfer_keep_alive::Dest,
                #[codec(compact)]
                pub value: transfer_keep_alive::Value,
            }
            pub mod transfer_keep_alive {
                use super::runtime_types;
                pub type Dest = ::subxt::utils::MultiAddress<::subxt::utils::AccountId32, ()>;
                pub type Value = ::core::primitive::u128;
            }
            impl TransferKeepAlive {
                const PALLET_NAME: &'static str = "Balances";
                const CALL_NAME: &'static str = "transfer_keep_alive";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for TransferKeepAlive {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Transfer the entire transferable balance from the caller account."]
            #[doc = ""]
            #[doc = "NOTE: This function only attempts to transfer _transferable_ balances. This means that"]
            #[doc = "any locked, reserved, or existential deposits (when `keep_alive` is `true`), will not be"]
            #[doc = "transferred by this function. To ensure that this function results in a killed account,"]
            #[doc = "you might need to prepare the account by removing any reference counters, storage"]
            #[doc = "deposits, etc..."]
            #[doc = ""]
            #[doc = "The dispatch origin of this call must be Signed."]
            #[doc = ""]
            #[doc = "- `dest`: The recipient of the transfer."]
            #[doc = "- `keep_alive`: A boolean to determine if the `transfer_all` operation should send all"]
            #[doc = "  of the funds the account has, causing the sender account to be killed (false), or"]
            #[doc = "  transfer everything except at least the existential deposit, which will guarantee to"]
            #[doc = "  keep the sender account alive (true)."]
            pub struct TransferAll {
                pub dest: transfer_all::Dest,
                pub keep_alive: transfer_all::KeepAlive,
            }
            pub mod transfer_all {
                use super::runtime_types;
                pub type Dest = ::subxt::utils::MultiAddress<::subxt::utils::AccountId32, ()>;
                pub type KeepAlive = ::core::primitive::bool;
            }
            impl TransferAll {
                const PALLET_NAME: &'static str = "Balances";
                const CALL_NAME: &'static str = "transfer_all";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for TransferAll {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Unreserve some balance from a user by force."]
            #[doc = ""]
            #[doc = "Can only be called by ROOT."]
            pub struct ForceUnreserve {
                pub who: force_unreserve::Who,
                pub amount: force_unreserve::Amount,
            }
            pub mod force_unreserve {
                use super::runtime_types;
                pub type Who = ::subxt::utils::MultiAddress<::subxt::utils::AccountId32, ()>;
                pub type Amount = ::core::primitive::u128;
            }
            impl ForceUnreserve {
                const PALLET_NAME: &'static str = "Balances";
                const CALL_NAME: &'static str = "force_unreserve";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for ForceUnreserve {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Upgrade a specified account."]
            #[doc = ""]
            #[doc = "- `origin`: Must be `Signed`."]
            #[doc = "- `who`: The account to be upgraded."]
            #[doc = ""]
            #[doc = "This will waive the transaction fee if at least all but 10% of the accounts needed to"]
            #[doc = "be upgraded. (We let some not have to be upgraded just in order to allow for the"]
            #[doc = "possibility of churn)."]
            pub struct UpgradeAccounts {
                pub who: upgrade_accounts::Who,
            }
            pub mod upgrade_accounts {
                use super::runtime_types;
                pub type Who = ::subxt::alloc::vec::Vec<::subxt::utils::AccountId32>;
            }
            impl UpgradeAccounts {
                const PALLET_NAME: &'static str = "Balances";
                const CALL_NAME: &'static str = "upgrade_accounts";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for UpgradeAccounts {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Set the regular balance of a given account."]
            #[doc = ""]
            #[doc = "The dispatch origin for this call is `root`."]
            pub struct ForceSetBalance {
                pub who: force_set_balance::Who,
                #[codec(compact)]
                pub new_free: force_set_balance::NewFree,
            }
            pub mod force_set_balance {
                use super::runtime_types;
                pub type Who = ::subxt::utils::MultiAddress<::subxt::utils::AccountId32, ()>;
                pub type NewFree = ::core::primitive::u128;
            }
            impl ForceSetBalance {
                const PALLET_NAME: &'static str = "Balances";
                const CALL_NAME: &'static str = "force_set_balance";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for ForceSetBalance {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Adjust the total issuance in a saturating way."]
            #[doc = ""]
            #[doc = "Can only be called by root and always needs a positive `delta`."]
            #[doc = ""]
            #[doc = "# Example"]
            pub struct ForceAdjustTotalIssuance {
                pub direction: force_adjust_total_issuance::Direction,
                #[codec(compact)]
                pub delta: force_adjust_total_issuance::Delta,
            }
            pub mod force_adjust_total_issuance {
                use super::runtime_types;
                pub type Direction = runtime_types::pallet_balances::types::AdjustmentDirection;
                pub type Delta = ::core::primitive::u128;
            }
            impl ForceAdjustTotalIssuance {
                const PALLET_NAME: &'static str = "Balances";
                const CALL_NAME: &'static str = "force_adjust_total_issuance";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for ForceAdjustTotalIssuance {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Burn the specified liquid free balance from the origin account."]
            #[doc = ""]
            #[doc = "If the origin's account ends up below the existential deposit as a result"]
            #[doc = "of the burn and `keep_alive` is false, the account will be reaped."]
            #[doc = ""]
            #[doc = "Unlike sending funds to a _burn_ address, which merely makes the funds inaccessible,"]
            #[doc = "this `burn` operation will reduce total issuance by the amount _burned_."]
            pub struct Burn {
                #[codec(compact)]
                pub value: burn::Value,
                pub keep_alive: burn::KeepAlive,
            }
            pub mod burn {
                use super::runtime_types;
                pub type Value = ::core::primitive::u128;
                pub type KeepAlive = ::core::primitive::bool;
            }
            impl Burn {
                const PALLET_NAME: &'static str = "Balances";
                const CALL_NAME: &'static str = "burn";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for Burn {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            pub mod api {
                pub struct TransactionApi;
                impl TransactionApi {
                    #[doc = "Transfer some liquid free balance to another account."]
                    #[doc = ""]
                    #[doc = "`transfer_allow_death` will set the `FreeBalance` of the sender and receiver."]
                    #[doc = "If the sender's account is below the existential deposit as a result"]
                    #[doc = "of the transfer, the account will be reaped."]
                    #[doc = ""]
                    #[doc = "The dispatch origin for this call must be `Signed` by the transactor."]
                    pub fn transfer_allow_death(
                        &self,
                        dest: super::transfer_allow_death::Dest,
                        value: super::transfer_allow_death::Value,
                    ) -> ::subxt::transactions::StaticPayload<super::TransferAllowDeath>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Balances",
                            "transfer_allow_death",
                            super::TransferAllowDeath { dest, value },
                            [
                                51u8, 166u8, 195u8, 10u8, 139u8, 218u8, 55u8, 130u8, 6u8, 194u8,
                                35u8, 140u8, 27u8, 205u8, 214u8, 222u8, 102u8, 43u8, 143u8, 145u8,
                                86u8, 219u8, 210u8, 147u8, 13u8, 39u8, 51u8, 21u8, 237u8, 179u8,
                                132u8, 130u8,
                            ],
                        )
                    }
                    #[doc = "Exactly as `transfer_allow_death`, except the origin must be root and the source account"]
                    #[doc = "may be specified."]
                    pub fn force_transfer(
                        &self,
                        source: super::force_transfer::Source,
                        dest: super::force_transfer::Dest,
                        value: super::force_transfer::Value,
                    ) -> ::subxt::transactions::StaticPayload<super::ForceTransfer>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Balances",
                            "force_transfer",
                            super::ForceTransfer {
                                source,
                                dest,
                                value,
                            },
                            [
                                154u8, 93u8, 222u8, 27u8, 12u8, 248u8, 63u8, 213u8, 224u8, 86u8,
                                250u8, 153u8, 249u8, 102u8, 83u8, 160u8, 79u8, 125u8, 105u8, 222u8,
                                77u8, 180u8, 90u8, 105u8, 81u8, 217u8, 60u8, 25u8, 213u8, 51u8,
                                185u8, 96u8,
                            ],
                        )
                    }
                    #[doc = "Same as the [`transfer_allow_death`] call, but with a check that the transfer will not"]
                    #[doc = "kill the origin account."]
                    #[doc = ""]
                    #[doc = "99% of the time you want [`transfer_allow_death`] instead."]
                    #[doc = ""]
                    #[doc = "[`transfer_allow_death`]: struct.Pallet.html#method.transfer"]
                    pub fn transfer_keep_alive(
                        &self,
                        dest: super::transfer_keep_alive::Dest,
                        value: super::transfer_keep_alive::Value,
                    ) -> ::subxt::transactions::StaticPayload<super::TransferKeepAlive>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Balances",
                            "transfer_keep_alive",
                            super::TransferKeepAlive { dest, value },
                            [
                                245u8, 14u8, 190u8, 193u8, 32u8, 210u8, 74u8, 92u8, 25u8, 182u8,
                                76u8, 55u8, 247u8, 83u8, 114u8, 75u8, 143u8, 236u8, 117u8, 25u8,
                                54u8, 157u8, 208u8, 207u8, 233u8, 89u8, 70u8, 161u8, 235u8, 242u8,
                                222u8, 59u8,
                            ],
                        )
                    }
                    #[doc = "Transfer the entire transferable balance from the caller account."]
                    #[doc = ""]
                    #[doc = "NOTE: This function only attempts to transfer _transferable_ balances. This means that"]
                    #[doc = "any locked, reserved, or existential deposits (when `keep_alive` is `true`), will not be"]
                    #[doc = "transferred by this function. To ensure that this function results in a killed account,"]
                    #[doc = "you might need to prepare the account by removing any reference counters, storage"]
                    #[doc = "deposits, etc..."]
                    #[doc = ""]
                    #[doc = "The dispatch origin of this call must be Signed."]
                    #[doc = ""]
                    #[doc = "- `dest`: The recipient of the transfer."]
                    #[doc = "- `keep_alive`: A boolean to determine if the `transfer_all` operation should send all"]
                    #[doc = "  of the funds the account has, causing the sender account to be killed (false), or"]
                    #[doc = "  transfer everything except at least the existential deposit, which will guarantee to"]
                    #[doc = "  keep the sender account alive (true)."]
                    pub fn transfer_all(
                        &self,
                        dest: super::transfer_all::Dest,
                        keep_alive: super::transfer_all::KeepAlive,
                    ) -> ::subxt::transactions::StaticPayload<super::TransferAll>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Balances",
                            "transfer_all",
                            super::TransferAll { dest, keep_alive },
                            [
                                105u8, 132u8, 49u8, 144u8, 195u8, 250u8, 34u8, 46u8, 213u8, 248u8,
                                112u8, 188u8, 81u8, 228u8, 136u8, 18u8, 67u8, 172u8, 37u8, 38u8,
                                238u8, 9u8, 34u8, 15u8, 67u8, 34u8, 148u8, 195u8, 223u8, 29u8,
                                154u8, 6u8,
                            ],
                        )
                    }
                    #[doc = "Unreserve some balance from a user by force."]
                    #[doc = ""]
                    #[doc = "Can only be called by ROOT."]
                    pub fn force_unreserve(
                        &self,
                        who: super::force_unreserve::Who,
                        amount: super::force_unreserve::Amount,
                    ) -> ::subxt::transactions::StaticPayload<super::ForceUnreserve>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Balances",
                            "force_unreserve",
                            super::ForceUnreserve { who, amount },
                            [
                                142u8, 151u8, 64u8, 205u8, 46u8, 64u8, 62u8, 122u8, 108u8, 49u8,
                                223u8, 140u8, 120u8, 153u8, 35u8, 165u8, 187u8, 38u8, 157u8, 200u8,
                                123u8, 199u8, 198u8, 168u8, 208u8, 159u8, 39u8, 134u8, 92u8, 103u8,
                                84u8, 171u8,
                            ],
                        )
                    }
                    #[doc = "Upgrade a specified account."]
                    #[doc = ""]
                    #[doc = "- `origin`: Must be `Signed`."]
                    #[doc = "- `who`: The account to be upgraded."]
                    #[doc = ""]
                    #[doc = "This will waive the transaction fee if at least all but 10% of the accounts needed to"]
                    #[doc = "be upgraded. (We let some not have to be upgraded just in order to allow for the"]
                    #[doc = "possibility of churn)."]
                    pub fn upgrade_accounts(
                        &self,
                        who: super::upgrade_accounts::Who,
                    ) -> ::subxt::transactions::StaticPayload<super::UpgradeAccounts>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Balances",
                            "upgrade_accounts",
                            super::UpgradeAccounts { who },
                            [
                                66u8, 200u8, 179u8, 104u8, 65u8, 2u8, 101u8, 56u8, 130u8, 161u8,
                                224u8, 233u8, 255u8, 124u8, 70u8, 122u8, 8u8, 49u8, 103u8, 178u8,
                                68u8, 47u8, 214u8, 166u8, 217u8, 116u8, 178u8, 50u8, 212u8, 164u8,
                                98u8, 226u8,
                            ],
                        )
                    }
                    #[doc = "Set the regular balance of a given account."]
                    #[doc = ""]
                    #[doc = "The dispatch origin for this call is `root`."]
                    pub fn force_set_balance(
                        &self,
                        who: super::force_set_balance::Who,
                        new_free: super::force_set_balance::NewFree,
                    ) -> ::subxt::transactions::StaticPayload<super::ForceSetBalance>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Balances",
                            "force_set_balance",
                            super::ForceSetBalance { who, new_free },
                            [
                                114u8, 229u8, 59u8, 204u8, 180u8, 83u8, 17u8, 4u8, 59u8, 4u8, 55u8,
                                39u8, 151u8, 196u8, 124u8, 60u8, 209u8, 65u8, 193u8, 11u8, 44u8,
                                164u8, 116u8, 93u8, 169u8, 30u8, 199u8, 165u8, 55u8, 231u8, 223u8,
                                43u8,
                            ],
                        )
                    }
                    #[doc = "Adjust the total issuance in a saturating way."]
                    #[doc = ""]
                    #[doc = "Can only be called by root and always needs a positive `delta`."]
                    #[doc = ""]
                    #[doc = "# Example"]
                    pub fn force_adjust_total_issuance(
                        &self,
                        direction: super::force_adjust_total_issuance::Direction,
                        delta: super::force_adjust_total_issuance::Delta,
                    ) -> ::subxt::transactions::StaticPayload<super::ForceAdjustTotalIssuance>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Balances",
                            "force_adjust_total_issuance",
                            super::ForceAdjustTotalIssuance { direction, delta },
                            [
                                208u8, 134u8, 56u8, 133u8, 232u8, 164u8, 10u8, 213u8, 53u8, 193u8,
                                190u8, 63u8, 236u8, 186u8, 96u8, 122u8, 104u8, 87u8, 173u8, 38u8,
                                58u8, 176u8, 21u8, 78u8, 42u8, 106u8, 46u8, 248u8, 251u8, 190u8,
                                150u8, 202u8,
                            ],
                        )
                    }
                    #[doc = "Burn the specified liquid free balance from the origin account."]
                    #[doc = ""]
                    #[doc = "If the origin's account ends up below the existential deposit as a result"]
                    #[doc = "of the burn and `keep_alive` is false, the account will be reaped."]
                    #[doc = ""]
                    #[doc = "Unlike sending funds to a _burn_ address, which merely makes the funds inaccessible,"]
                    #[doc = "this `burn` operation will reduce total issuance by the amount _burned_."]
                    pub fn burn(
                        &self,
                        value: super::burn::Value,
                        keep_alive: super::burn::KeepAlive,
                    ) -> ::subxt::transactions::StaticPayload<super::Burn> {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Balances",
                            "burn",
                            super::Burn { value, keep_alive },
                            [
                                176u8, 64u8, 7u8, 109u8, 16u8, 44u8, 145u8, 125u8, 147u8, 152u8,
                                130u8, 114u8, 221u8, 201u8, 150u8, 162u8, 118u8, 71u8, 52u8, 92u8,
                                240u8, 116u8, 203u8, 98u8, 5u8, 22u8, 43u8, 102u8, 94u8, 208u8,
                                101u8, 57u8,
                            ],
                        )
                    }
                }
            }
        }
        #[doc = "The `Event` enum of this pallet"]
        pub type Event = runtime_types::pallet_balances::pallet::Event;
        pub mod events {
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "An account was created with some free balance."]
            pub struct Endowed {
                pub account: endowed::Account,
                pub free_balance: endowed::FreeBalance,
            }
            pub mod endowed {
                use super::runtime_types;
                pub type Account = ::subxt::utils::AccountId32;
                pub type FreeBalance = ::core::primitive::u128;
            }
            impl Endowed {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Endowed";
            }
            impl ::subxt::events::DecodeAsEvent for Endowed {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "An account was removed whose balance was non-zero but below ExistentialDeposit,"]
            #[doc = "resulting in an outright loss."]
            pub struct DustLost {
                pub account: dust_lost::Account,
                pub amount: dust_lost::Amount,
            }
            pub mod dust_lost {
                use super::runtime_types;
                pub type Account = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl DustLost {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "DustLost";
            }
            impl ::subxt::events::DecodeAsEvent for DustLost {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Transfer succeeded."]
            pub struct Transfer {
                pub from: transfer::From,
                pub to: transfer::To,
                pub amount: transfer::Amount,
            }
            pub mod transfer {
                use super::runtime_types;
                pub type From = ::subxt::utils::AccountId32;
                pub type To = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl Transfer {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Transfer";
            }
            impl ::subxt::events::DecodeAsEvent for Transfer {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "A balance was set by root."]
            pub struct BalanceSet {
                pub who: balance_set::Who,
                pub free: balance_set::Free,
            }
            pub mod balance_set {
                use super::runtime_types;
                pub type Who = ::subxt::utils::AccountId32;
                pub type Free = ::core::primitive::u128;
            }
            impl BalanceSet {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "BalanceSet";
            }
            impl ::subxt::events::DecodeAsEvent for BalanceSet {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Some balance was reserved (moved from free to reserved)."]
            pub struct Reserved {
                pub who: reserved::Who,
                pub amount: reserved::Amount,
            }
            pub mod reserved {
                use super::runtime_types;
                pub type Who = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl Reserved {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Reserved";
            }
            impl ::subxt::events::DecodeAsEvent for Reserved {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Some balance was unreserved (moved from reserved to free)."]
            pub struct Unreserved {
                pub who: unreserved::Who,
                pub amount: unreserved::Amount,
            }
            pub mod unreserved {
                use super::runtime_types;
                pub type Who = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl Unreserved {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Unreserved";
            }
            impl ::subxt::events::DecodeAsEvent for Unreserved {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Some balance was moved from the reserve of the first account to the second account."]
            #[doc = "Final argument indicates the destination balance type."]
            pub struct ReserveRepatriated {
                pub from: reserve_repatriated::From,
                pub to: reserve_repatriated::To,
                pub amount: reserve_repatriated::Amount,
                pub destination_status: reserve_repatriated::DestinationStatus,
            }
            pub mod reserve_repatriated {
                use super::runtime_types;
                pub type From = ::subxt::utils::AccountId32;
                pub type To = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
                pub type DestinationStatus =
                    runtime_types::frame_support::traits::tokens::misc::BalanceStatus;
            }
            impl ReserveRepatriated {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "ReserveRepatriated";
            }
            impl ::subxt::events::DecodeAsEvent for ReserveRepatriated {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Some amount was deposited (e.g. for transaction fees)."]
            pub struct Deposit {
                pub who: deposit::Who,
                pub amount: deposit::Amount,
            }
            pub mod deposit {
                use super::runtime_types;
                pub type Who = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl Deposit {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Deposit";
            }
            impl ::subxt::events::DecodeAsEvent for Deposit {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Some amount was withdrawn from the account (e.g. for transaction fees)."]
            pub struct Withdraw {
                pub who: withdraw::Who,
                pub amount: withdraw::Amount,
            }
            pub mod withdraw {
                use super::runtime_types;
                pub type Who = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl Withdraw {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Withdraw";
            }
            impl ::subxt::events::DecodeAsEvent for Withdraw {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Some amount was removed from the account (e.g. for misbehavior)."]
            pub struct Slashed {
                pub who: slashed::Who,
                pub amount: slashed::Amount,
            }
            pub mod slashed {
                use super::runtime_types;
                pub type Who = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl Slashed {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Slashed";
            }
            impl ::subxt::events::DecodeAsEvent for Slashed {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Some amount was minted into an account."]
            pub struct Minted {
                pub who: minted::Who,
                pub amount: minted::Amount,
            }
            pub mod minted {
                use super::runtime_types;
                pub type Who = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl Minted {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Minted";
            }
            impl ::subxt::events::DecodeAsEvent for Minted {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Some credit was balanced and added to the TotalIssuance."]
            pub struct MintedCredit {
                pub amount: minted_credit::Amount,
            }
            pub mod minted_credit {
                use super::runtime_types;
                pub type Amount = ::core::primitive::u128;
            }
            impl MintedCredit {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "MintedCredit";
            }
            impl ::subxt::events::DecodeAsEvent for MintedCredit {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Some amount was burned from an account."]
            pub struct Burned {
                pub who: burned::Who,
                pub amount: burned::Amount,
            }
            pub mod burned {
                use super::runtime_types;
                pub type Who = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl Burned {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Burned";
            }
            impl ::subxt::events::DecodeAsEvent for Burned {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Some debt has been dropped from the Total Issuance."]
            pub struct BurnedDebt {
                pub amount: burned_debt::Amount,
            }
            pub mod burned_debt {
                use super::runtime_types;
                pub type Amount = ::core::primitive::u128;
            }
            impl BurnedDebt {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "BurnedDebt";
            }
            impl ::subxt::events::DecodeAsEvent for BurnedDebt {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Some amount was suspended from an account (it can be restored later)."]
            pub struct Suspended {
                pub who: suspended::Who,
                pub amount: suspended::Amount,
            }
            pub mod suspended {
                use super::runtime_types;
                pub type Who = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl Suspended {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Suspended";
            }
            impl ::subxt::events::DecodeAsEvent for Suspended {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Some amount was restored into an account."]
            pub struct Restored {
                pub who: restored::Who,
                pub amount: restored::Amount,
            }
            pub mod restored {
                use super::runtime_types;
                pub type Who = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl Restored {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Restored";
            }
            impl ::subxt::events::DecodeAsEvent for Restored {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "An account was upgraded."]
            pub struct Upgraded {
                pub who: upgraded::Who,
            }
            pub mod upgraded {
                use super::runtime_types;
                pub type Who = ::subxt::utils::AccountId32;
            }
            impl Upgraded {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Upgraded";
            }
            impl ::subxt::events::DecodeAsEvent for Upgraded {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Total issuance was increased by `amount`, creating a credit to be balanced."]
            pub struct Issued {
                pub amount: issued::Amount,
            }
            pub mod issued {
                use super::runtime_types;
                pub type Amount = ::core::primitive::u128;
            }
            impl Issued {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Issued";
            }
            impl ::subxt::events::DecodeAsEvent for Issued {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Total issuance was decreased by `amount`, creating a debt to be balanced."]
            pub struct Rescinded {
                pub amount: rescinded::Amount,
            }
            pub mod rescinded {
                use super::runtime_types;
                pub type Amount = ::core::primitive::u128;
            }
            impl Rescinded {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Rescinded";
            }
            impl ::subxt::events::DecodeAsEvent for Rescinded {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Some balance was locked."]
            pub struct Locked {
                pub who: locked::Who,
                pub amount: locked::Amount,
            }
            pub mod locked {
                use super::runtime_types;
                pub type Who = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl Locked {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Locked";
            }
            impl ::subxt::events::DecodeAsEvent for Locked {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Some balance was unlocked."]
            pub struct Unlocked {
                pub who: unlocked::Who,
                pub amount: unlocked::Amount,
            }
            pub mod unlocked {
                use super::runtime_types;
                pub type Who = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl Unlocked {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Unlocked";
            }
            impl ::subxt::events::DecodeAsEvent for Unlocked {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Some balance was frozen."]
            pub struct Frozen {
                pub who: frozen::Who,
                pub amount: frozen::Amount,
            }
            pub mod frozen {
                use super::runtime_types;
                pub type Who = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl Frozen {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Frozen";
            }
            impl ::subxt::events::DecodeAsEvent for Frozen {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Some balance was thawed."]
            pub struct Thawed {
                pub who: thawed::Who,
                pub amount: thawed::Amount,
            }
            pub mod thawed {
                use super::runtime_types;
                pub type Who = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl Thawed {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Thawed";
            }
            impl ::subxt::events::DecodeAsEvent for Thawed {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "The `TotalIssuance` was forcefully changed."]
            pub struct TotalIssuanceForced {
                pub old: total_issuance_forced::Old,
                pub new: total_issuance_forced::New,
            }
            pub mod total_issuance_forced {
                use super::runtime_types;
                pub type Old = ::core::primitive::u128;
                pub type New = ::core::primitive::u128;
            }
            impl TotalIssuanceForced {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "TotalIssuanceForced";
            }
            impl ::subxt::events::DecodeAsEvent for TotalIssuanceForced {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Some balance was placed on hold."]
            pub struct Held {
                pub reason: held::Reason,
                pub who: held::Who,
                pub amount: held::Amount,
            }
            pub mod held {
                use super::runtime_types;
                pub type Reason = runtime_types::acuity_runtime::RuntimeHoldReason;
                pub type Who = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl Held {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Held";
            }
            impl ::subxt::events::DecodeAsEvent for Held {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Held balance was burned from an account."]
            pub struct BurnedHeld {
                pub reason: burned_held::Reason,
                pub who: burned_held::Who,
                pub amount: burned_held::Amount,
            }
            pub mod burned_held {
                use super::runtime_types;
                pub type Reason = runtime_types::acuity_runtime::RuntimeHoldReason;
                pub type Who = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl BurnedHeld {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "BurnedHeld";
            }
            impl ::subxt::events::DecodeAsEvent for BurnedHeld {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "A transfer of `amount` on hold from `source` to `dest` was initiated."]
            pub struct TransferOnHold {
                pub reason: transfer_on_hold::Reason,
                pub source: transfer_on_hold::Source,
                pub dest: transfer_on_hold::Dest,
                pub amount: transfer_on_hold::Amount,
            }
            pub mod transfer_on_hold {
                use super::runtime_types;
                pub type Reason = runtime_types::acuity_runtime::RuntimeHoldReason;
                pub type Source = ::subxt::utils::AccountId32;
                pub type Dest = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl TransferOnHold {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "TransferOnHold";
            }
            impl ::subxt::events::DecodeAsEvent for TransferOnHold {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "The `transferred` balance is placed on hold at the `dest` account."]
            pub struct TransferAndHold {
                pub reason: transfer_and_hold::Reason,
                pub source: transfer_and_hold::Source,
                pub dest: transfer_and_hold::Dest,
                pub transferred: transfer_and_hold::Transferred,
            }
            pub mod transfer_and_hold {
                use super::runtime_types;
                pub type Reason = runtime_types::acuity_runtime::RuntimeHoldReason;
                pub type Source = ::subxt::utils::AccountId32;
                pub type Dest = ::subxt::utils::AccountId32;
                pub type Transferred = ::core::primitive::u128;
            }
            impl TransferAndHold {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "TransferAndHold";
            }
            impl ::subxt::events::DecodeAsEvent for TransferAndHold {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Some balance was released from hold."]
            pub struct Released {
                pub reason: released::Reason,
                pub who: released::Who,
                pub amount: released::Amount,
            }
            pub mod released {
                use super::runtime_types;
                pub type Reason = runtime_types::acuity_runtime::RuntimeHoldReason;
                pub type Who = ::subxt::utils::AccountId32;
                pub type Amount = ::core::primitive::u128;
            }
            impl Released {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Released";
            }
            impl ::subxt::events::DecodeAsEvent for Released {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "An unexpected/defensive event was triggered."]
            pub struct Unexpected(pub unexpected::Field0);
            pub mod unexpected {
                use super::runtime_types;
                pub type Field0 = runtime_types::pallet_balances::pallet::UnexpectedKind;
            }
            impl Unexpected {
                const PALLET_NAME: &'static str = "Balances";
                const EVENT_NAME: &'static str = "Unexpected";
            }
            impl ::subxt::events::DecodeAsEvent for Unexpected {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
        }
        pub mod storage {
            use super::root_mod;
            use super::runtime_types;
            pub struct StorageApi;
            impl StorageApi {
                #[doc = " The total units issued in the system."]
                pub fn total_issuance(
                    &self,
                ) -> ::subxt::storage::StaticAddress<(), total_issuance::Output, ::subxt::utils::Yes>
                {
                    ::subxt::storage::StaticAddress::new_static(
                        "Balances",
                        "TotalIssuance",
                        [
                            138u8, 120u8, 138u8, 119u8, 4u8, 166u8, 22u8, 216u8, 227u8, 249u8,
                            161u8, 193u8, 54u8, 68u8, 55u8, 74u8, 230u8, 68u8, 131u8, 253u8, 146u8,
                            73u8, 54u8, 85u8, 212u8, 83u8, 162u8, 188u8, 171u8, 5u8, 232u8, 21u8,
                        ],
                    )
                }
                #[doc = " The total units of outstanding deactivated balance in the system."]
                pub fn inactive_issuance(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (),
                    inactive_issuance::Output,
                    ::subxt::utils::Yes,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "Balances",
                        "InactiveIssuance",
                        [
                            97u8, 194u8, 82u8, 3u8, 40u8, 108u8, 109u8, 245u8, 175u8, 189u8, 212u8,
                            193u8, 229u8, 82u8, 107u8, 169u8, 9u8, 176u8, 124u8, 102u8, 151u8,
                            98u8, 87u8, 194u8, 82u8, 130u8, 41u8, 137u8, 3u8, 230u8, 145u8, 58u8,
                        ],
                    )
                }
                #[doc = " The Balances pallet example of storing the balance of an account."]
                #[doc = ""]
                #[doc = " # Example"]
                #[doc = ""]
                #[doc = " ```nocompile"]
                #[doc = "  impl pallet_balances::Config for Runtime {"]
                #[doc = "    type AccountStore = StorageMapShim<Self::Account<Runtime>, frame_system::Provider<Runtime>, AccountId, Self::AccountData<Balance>>"]
                #[doc = "  }"]
                #[doc = " ```"]
                #[doc = ""]
                #[doc = " You can also store the balance of an account in the `System` pallet."]
                #[doc = ""]
                #[doc = " # Example"]
                #[doc = ""]
                #[doc = " ```nocompile"]
                #[doc = "  impl pallet_balances::Config for Runtime {"]
                #[doc = "   type AccountStore = System"]
                #[doc = "  }"]
                #[doc = " ```"]
                #[doc = ""]
                #[doc = " But this comes with tradeoffs, storing account balances in the system pallet stores"]
                #[doc = " `frame_system` data alongside the account data contrary to storing account balances in the"]
                #[doc = " `Balances` pallet, which uses a `StorageMap` to store balances data only."]
                #[doc = " NOTE: This is only used in the case that this pallet is used to store balances."]
                pub fn account(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (account::input::Param0,),
                    account::Output,
                    ::subxt::utils::Maybe,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "Balances",
                        "Account",
                        [
                            14u8, 88u8, 174u8, 192u8, 241u8, 142u8, 159u8, 255u8, 178u8, 117u8,
                            55u8, 78u8, 218u8, 161u8, 146u8, 139u8, 170u8, 180u8, 187u8, 177u8,
                            89u8, 157u8, 91u8, 225u8, 90u8, 174u8, 247u8, 47u8, 47u8, 23u8, 234u8,
                            50u8,
                        ],
                    )
                }
                #[doc = " Any liquidity locks on some account balances."]
                #[doc = " NOTE: Should only be accessed when setting, changing and freeing a lock."]
                #[doc = ""]
                #[doc = " Use of locks is deprecated in favour of freezes. See `https://github.com/paritytech/substrate/pull/12951/`"]
                pub fn locks(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (locks::input::Param0,),
                    locks::Output,
                    ::subxt::utils::Maybe,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "Balances",
                        "Locks",
                        [
                            201u8, 50u8, 65u8, 126u8, 43u8, 153u8, 207u8, 145u8, 240u8, 59u8,
                            160u8, 111u8, 144u8, 245u8, 193u8, 13u8, 227u8, 118u8, 72u8, 168u8,
                            37u8, 147u8, 139u8, 221u8, 36u8, 177u8, 202u8, 209u8, 152u8, 122u8,
                            250u8, 89u8,
                        ],
                    )
                }
                #[doc = " Named reserves on some account balances."]
                #[doc = ""]
                #[doc = " Use of reserves is deprecated in favour of holds. See `https://github.com/paritytech/substrate/pull/12951/`"]
                pub fn reserves(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (reserves::input::Param0,),
                    reserves::Output,
                    ::subxt::utils::Maybe,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "Balances",
                        "Reserves",
                        [
                            242u8, 49u8, 250u8, 113u8, 214u8, 13u8, 201u8, 251u8, 161u8, 201u8,
                            81u8, 43u8, 122u8, 155u8, 190u8, 214u8, 210u8, 26u8, 104u8, 107u8,
                            113u8, 108u8, 113u8, 171u8, 174u8, 31u8, 169u8, 220u8, 98u8, 39u8,
                            68u8, 245u8,
                        ],
                    )
                }
                #[doc = " Holds on account balances."]
                pub fn holds(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (holds::input::Param0,),
                    holds::Output,
                    ::subxt::utils::Maybe,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "Balances",
                        "Holds",
                        [
                            127u8, 213u8, 56u8, 163u8, 150u8, 56u8, 111u8, 121u8, 191u8, 137u8,
                            59u8, 114u8, 75u8, 24u8, 70u8, 242u8, 138u8, 133u8, 215u8, 74u8, 230u8,
                            129u8, 232u8, 84u8, 145u8, 55u8, 37u8, 63u8, 69u8, 59u8, 121u8, 243u8,
                        ],
                    )
                }
                #[doc = " Freeze locks on account balances."]
                pub fn freezes(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (freezes::input::Param0,),
                    freezes::Output,
                    ::subxt::utils::Maybe,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "Balances",
                        "Freezes",
                        [
                            41u8, 196u8, 69u8, 26u8, 201u8, 141u8, 252u8, 255u8, 78u8, 216u8,
                            102u8, 207u8, 133u8, 185u8, 86u8, 18u8, 79u8, 137u8, 132u8, 92u8,
                            228u8, 237u8, 91u8, 125u8, 25u8, 111u8, 127u8, 212u8, 215u8, 114u8,
                            219u8, 72u8,
                        ],
                    )
                }
            }
            pub mod total_issuance {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = ::core::primitive::u128;
            }
            pub mod inactive_issuance {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = ::core::primitive::u128;
            }
            pub mod account {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                    pub type Param0 = ::subxt::utils::AccountId32;
                }
                pub type Output =
                    runtime_types::pallet_balances::types::AccountData<::core::primitive::u128>;
            }
            pub mod locks {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                    pub type Param0 = ::subxt::utils::AccountId32;
                }
                pub type Output =
                    runtime_types::bounded_collections::weak_bounded_vec::WeakBoundedVec<
                        runtime_types::pallet_balances::types::BalanceLock<::core::primitive::u128>,
                    >;
            }
            pub mod reserves {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                    pub type Param0 = ::subxt::utils::AccountId32;
                }
                pub type Output = runtime_types::bounded_collections::bounded_vec::BoundedVec<
                    runtime_types::pallet_balances::types::ReserveData<(), ::core::primitive::u128>,
                >;
            }
            pub mod holds {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                    pub type Param0 = ::subxt::utils::AccountId32;
                }
                pub type Output = runtime_types::bounded_collections::bounded_vec::BoundedVec<
                    runtime_types::frame_support::traits::tokens::misc::IdAmount<
                        runtime_types::acuity_runtime::RuntimeHoldReason,
                        ::core::primitive::u128,
                    >,
                >;
            }
            pub mod freezes {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                    pub type Param0 = ::subxt::utils::AccountId32;
                }
                pub type Output = runtime_types::bounded_collections::bounded_vec::BoundedVec<
                    runtime_types::frame_support::traits::tokens::misc::IdAmount<
                        (),
                        ::core::primitive::u128,
                    >,
                >;
            }
        }
        pub mod constants {
            use super::runtime_types;
            pub struct ConstantsApi;
            impl ConstantsApi {
                #[doc = " The minimum amount required to keep an account open. MUST BE GREATER THAN ZERO!"]
                #[doc = ""]
                #[doc = " If you *really* need it to be zero, you can enable the feature `insecure_zero_ed` for"]
                #[doc = " this pallet. However, you do so at your own risk: this will open up a major DoS vector."]
                #[doc = " In case you have multiple sources of provider references, you may also get unexpected"]
                #[doc = " behaviour if you set this to zero."]
                #[doc = ""]
                #[doc = " Bottom line: Do yourself a favour and make it at least one!"]
                pub fn existential_deposit(
                    &self,
                ) -> ::subxt::constants::StaticAddress<::core::primitive::u128> {
                    ::subxt::constants::StaticAddress::new_static(
                        "Balances",
                        "ExistentialDeposit",
                        [
                            84u8, 157u8, 140u8, 4u8, 93u8, 57u8, 29u8, 133u8, 105u8, 200u8, 214u8,
                            27u8, 144u8, 208u8, 218u8, 160u8, 130u8, 109u8, 101u8, 54u8, 210u8,
                            136u8, 71u8, 63u8, 49u8, 237u8, 234u8, 15u8, 178u8, 98u8, 148u8, 156u8,
                        ],
                    )
                }
                #[doc = " The maximum number of locks that should exist on an account."]
                #[doc = " Not strictly enforced, but used for weight estimation."]
                #[doc = ""]
                #[doc = " Use of locks is deprecated in favour of freezes. See `https://github.com/paritytech/substrate/pull/12951/`"]
                pub fn max_locks(
                    &self,
                ) -> ::subxt::constants::StaticAddress<::core::primitive::u32> {
                    ::subxt::constants::StaticAddress::new_static(
                        "Balances",
                        "MaxLocks",
                        [
                            98u8, 252u8, 116u8, 72u8, 26u8, 180u8, 225u8, 83u8, 200u8, 157u8,
                            125u8, 151u8, 53u8, 76u8, 168u8, 26u8, 10u8, 9u8, 98u8, 68u8, 9u8,
                            178u8, 197u8, 113u8, 31u8, 79u8, 200u8, 90u8, 203u8, 100u8, 41u8,
                            145u8,
                        ],
                    )
                }
                #[doc = " The maximum number of named reserves that can exist on an account."]
                #[doc = ""]
                #[doc = " Use of reserves is deprecated in favour of holds. See `https://github.com/paritytech/substrate/pull/12951/`"]
                pub fn max_reserves(
                    &self,
                ) -> ::subxt::constants::StaticAddress<::core::primitive::u32> {
                    ::subxt::constants::StaticAddress::new_static(
                        "Balances",
                        "MaxReserves",
                        [
                            98u8, 252u8, 116u8, 72u8, 26u8, 180u8, 225u8, 83u8, 200u8, 157u8,
                            125u8, 151u8, 53u8, 76u8, 168u8, 26u8, 10u8, 9u8, 98u8, 68u8, 9u8,
                            178u8, 197u8, 113u8, 31u8, 79u8, 200u8, 90u8, 203u8, 100u8, 41u8,
                            145u8,
                        ],
                    )
                }
                #[doc = " The maximum number of individual freeze locks that can exist on an account at any time."]
                pub fn max_freezes(
                    &self,
                ) -> ::subxt::constants::StaticAddress<::core::primitive::u32> {
                    ::subxt::constants::StaticAddress::new_static(
                        "Balances",
                        "MaxFreezes",
                        [
                            98u8, 252u8, 116u8, 72u8, 26u8, 180u8, 225u8, 83u8, 200u8, 157u8,
                            125u8, 151u8, 53u8, 76u8, 168u8, 26u8, 10u8, 9u8, 98u8, 68u8, 9u8,
                            178u8, 197u8, 113u8, 31u8, 79u8, 200u8, 90u8, 203u8, 100u8, 41u8,
                            145u8,
                        ],
                    )
                }
            }
        }
    }
    pub mod sudo {
        use super::root_mod;
        use super::runtime_types;
        #[doc = "Error for the Sudo pallet."]
        pub type Error = runtime_types::pallet_sudo::pallet::Error;
        #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
        pub type Call = runtime_types::pallet_sudo::pallet::Call;
        pub mod calls {
            use super::root_mod;
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Authenticates the sudo key and dispatches a function call with `Root` origin."]
            pub struct Sudo {
                pub call: ::subxt::alloc::boxed::Box<sudo::Call>,
            }
            pub mod sudo {
                use super::runtime_types;
                pub type Call = runtime_types::acuity_runtime::RuntimeCall;
            }
            impl Sudo {
                const PALLET_NAME: &'static str = "Sudo";
                const CALL_NAME: &'static str = "sudo";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for Sudo {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Authenticates the sudo key and dispatches a function call with `Root` origin."]
            #[doc = "This function does not check the weight of the call, and instead allows the"]
            #[doc = "Sudo user to specify the weight of the call."]
            #[doc = ""]
            #[doc = "The dispatch origin for this call must be _Signed_."]
            pub struct SudoUncheckedWeight {
                pub call: ::subxt::alloc::boxed::Box<sudo_unchecked_weight::Call>,
                pub weight: sudo_unchecked_weight::Weight,
            }
            pub mod sudo_unchecked_weight {
                use super::runtime_types;
                pub type Call = runtime_types::acuity_runtime::RuntimeCall;
                pub type Weight = runtime_types::sp_weights::weight_v2::Weight;
            }
            impl SudoUncheckedWeight {
                const PALLET_NAME: &'static str = "Sudo";
                const CALL_NAME: &'static str = "sudo_unchecked_weight";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for SudoUncheckedWeight {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Authenticates the current sudo key and sets the given AccountId (`new`) as the new sudo"]
            #[doc = "key."]
            pub struct SetKey {
                pub new: set_key::New,
            }
            pub mod set_key {
                use super::runtime_types;
                pub type New = ::subxt::utils::MultiAddress<::subxt::utils::AccountId32, ()>;
            }
            impl SetKey {
                const PALLET_NAME: &'static str = "Sudo";
                const CALL_NAME: &'static str = "set_key";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for SetKey {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Authenticates the sudo key and dispatches a function call with `Signed` origin from"]
            #[doc = "a given account."]
            #[doc = ""]
            #[doc = "The dispatch origin for this call must be _Signed_."]
            pub struct SudoAs {
                pub who: sudo_as::Who,
                pub call: ::subxt::alloc::boxed::Box<sudo_as::Call>,
            }
            pub mod sudo_as {
                use super::runtime_types;
                pub type Who = ::subxt::utils::MultiAddress<::subxt::utils::AccountId32, ()>;
                pub type Call = runtime_types::acuity_runtime::RuntimeCall;
            }
            impl SudoAs {
                const PALLET_NAME: &'static str = "Sudo";
                const CALL_NAME: &'static str = "sudo_as";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for SudoAs {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Permanently removes the sudo key."]
            #[doc = ""]
            #[doc = "**This cannot be un-done.**"]
            pub struct RemoveKey;
            impl RemoveKey {
                const PALLET_NAME: &'static str = "Sudo";
                const CALL_NAME: &'static str = "remove_key";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for RemoveKey {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            pub mod api {
                pub struct TransactionApi;
                impl TransactionApi {
                    #[doc = "Authenticates the sudo key and dispatches a function call with `Root` origin."]
                    pub fn sudo(
                        &self,
                        call: super::sudo::Call,
                    ) -> ::subxt::transactions::StaticPayload<super::Sudo> {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Sudo",
                            "sudo",
                            super::Sudo {
                                call: ::subxt::alloc::boxed::Box::new(call),
                            },
                            [
                                70u8, 130u8, 46u8, 245u8, 139u8, 31u8, 38u8, 158u8, 38u8, 145u8,
                                39u8, 117u8, 55u8, 131u8, 169u8, 26u8, 34u8, 96u8, 32u8, 163u8,
                                158u8, 229u8, 236u8, 230u8, 206u8, 1u8, 205u8, 6u8, 209u8, 16u8,
                                27u8, 98u8,
                            ],
                        )
                    }
                    #[doc = "Authenticates the sudo key and dispatches a function call with `Root` origin."]
                    #[doc = "This function does not check the weight of the call, and instead allows the"]
                    #[doc = "Sudo user to specify the weight of the call."]
                    #[doc = ""]
                    #[doc = "The dispatch origin for this call must be _Signed_."]
                    pub fn sudo_unchecked_weight(
                        &self,
                        call: super::sudo_unchecked_weight::Call,
                        weight: super::sudo_unchecked_weight::Weight,
                    ) -> ::subxt::transactions::StaticPayload<super::SudoUncheckedWeight>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Sudo",
                            "sudo_unchecked_weight",
                            super::SudoUncheckedWeight {
                                call: ::subxt::alloc::boxed::Box::new(call),
                                weight,
                            },
                            [
                                71u8, 20u8, 222u8, 239u8, 64u8, 237u8, 4u8, 211u8, 128u8, 214u8,
                                87u8, 134u8, 86u8, 65u8, 97u8, 21u8, 225u8, 225u8, 177u8, 33u8,
                                183u8, 79u8, 142u8, 27u8, 150u8, 107u8, 153u8, 20u8, 217u8, 149u8,
                                48u8, 222u8,
                            ],
                        )
                    }
                    #[doc = "Authenticates the current sudo key and sets the given AccountId (`new`) as the new sudo"]
                    #[doc = "key."]
                    pub fn set_key(
                        &self,
                        new: super::set_key::New,
                    ) -> ::subxt::transactions::StaticPayload<super::SetKey> {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Sudo",
                            "set_key",
                            super::SetKey { new },
                            [
                                9u8, 73u8, 39u8, 205u8, 188u8, 127u8, 143u8, 54u8, 128u8, 94u8,
                                8u8, 227u8, 197u8, 44u8, 70u8, 93u8, 228u8, 196u8, 64u8, 165u8,
                                226u8, 158u8, 101u8, 192u8, 22u8, 193u8, 102u8, 84u8, 21u8, 35u8,
                                92u8, 198u8,
                            ],
                        )
                    }
                    #[doc = "Authenticates the sudo key and dispatches a function call with `Signed` origin from"]
                    #[doc = "a given account."]
                    #[doc = ""]
                    #[doc = "The dispatch origin for this call must be _Signed_."]
                    pub fn sudo_as(
                        &self,
                        who: super::sudo_as::Who,
                        call: super::sudo_as::Call,
                    ) -> ::subxt::transactions::StaticPayload<super::SudoAs> {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Sudo",
                            "sudo_as",
                            super::SudoAs {
                                who,
                                call: ::subxt::alloc::boxed::Box::new(call),
                            },
                            [
                                66u8, 178u8, 57u8, 171u8, 99u8, 7u8, 202u8, 194u8, 230u8, 145u8,
                                136u8, 109u8, 200u8, 208u8, 53u8, 27u8, 123u8, 248u8, 223u8, 127u8,
                                97u8, 127u8, 77u8, 53u8, 2u8, 93u8, 175u8, 67u8, 172u8, 202u8,
                                116u8, 238u8,
                            ],
                        )
                    }
                    #[doc = "Permanently removes the sudo key."]
                    #[doc = ""]
                    #[doc = "**This cannot be un-done.**"]
                    pub fn remove_key(
                        &self,
                    ) -> ::subxt::transactions::StaticPayload<super::RemoveKey>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Sudo",
                            "remove_key",
                            super::RemoveKey {},
                            [
                                133u8, 253u8, 54u8, 175u8, 202u8, 239u8, 5u8, 198u8, 180u8, 138u8,
                                25u8, 28u8, 109u8, 40u8, 30u8, 56u8, 126u8, 100u8, 52u8, 205u8,
                                250u8, 191u8, 61u8, 195u8, 172u8, 142u8, 184u8, 239u8, 247u8, 10u8,
                                211u8, 79u8,
                            ],
                        )
                    }
                }
            }
        }
        #[doc = "The `Event` enum of this pallet"]
        pub type Event = runtime_types::pallet_sudo::pallet::Event;
        pub mod events {
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "A sudo call just took place."]
            pub struct Sudid {
                pub sudo_result: sudid::SudoResult,
            }
            pub mod sudid {
                use super::runtime_types;
                pub type SudoResult =
                    ::core::result::Result<(), runtime_types::sp_runtime::DispatchError>;
            }
            impl Sudid {
                const PALLET_NAME: &'static str = "Sudo";
                const EVENT_NAME: &'static str = "Sudid";
            }
            impl ::subxt::events::DecodeAsEvent for Sudid {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "The sudo key has been updated."]
            pub struct KeyChanged {
                pub old: key_changed::Old,
                pub new: key_changed::New,
            }
            pub mod key_changed {
                use super::runtime_types;
                pub type Old = ::core::option::Option<::subxt::utils::AccountId32>;
                pub type New = ::subxt::utils::AccountId32;
            }
            impl KeyChanged {
                const PALLET_NAME: &'static str = "Sudo";
                const EVENT_NAME: &'static str = "KeyChanged";
            }
            impl ::subxt::events::DecodeAsEvent for KeyChanged {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "The key was permanently removed."]
            pub struct KeyRemoved;
            impl KeyRemoved {
                const PALLET_NAME: &'static str = "Sudo";
                const EVENT_NAME: &'static str = "KeyRemoved";
            }
            impl ::subxt::events::DecodeAsEvent for KeyRemoved {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "A [sudo_as](Pallet::sudo_as) call just took place."]
            pub struct SudoAsDone {
                pub sudo_result: sudo_as_done::SudoResult,
            }
            pub mod sudo_as_done {
                use super::runtime_types;
                pub type SudoResult =
                    ::core::result::Result<(), runtime_types::sp_runtime::DispatchError>;
            }
            impl SudoAsDone {
                const PALLET_NAME: &'static str = "Sudo";
                const EVENT_NAME: &'static str = "SudoAsDone";
            }
            impl ::subxt::events::DecodeAsEvent for SudoAsDone {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
        }
        pub mod storage {
            use super::root_mod;
            use super::runtime_types;
            pub struct StorageApi;
            impl StorageApi {
                #[doc = " The `AccountId` of the sudo key."]
                pub fn key(
                    &self,
                ) -> ::subxt::storage::StaticAddress<(), key::Output, ::subxt::utils::Yes>
                {
                    ::subxt::storage::StaticAddress::new_static(
                        "Sudo",
                        "Key",
                        [
                            135u8, 9u8, 151u8, 148u8, 179u8, 127u8, 153u8, 3u8, 158u8, 91u8, 244u8,
                            242u8, 201u8, 85u8, 31u8, 10u8, 151u8, 125u8, 201u8, 113u8, 15u8,
                            104u8, 164u8, 246u8, 174u8, 14u8, 251u8, 184u8, 57u8, 223u8, 162u8,
                            139u8,
                        ],
                    )
                }
            }
            pub mod key {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = ::subxt::utils::AccountId32;
            }
        }
    }
    pub mod transaction_payment {
        use super::root_mod;
        use super::runtime_types;
        #[doc = "The `Event` enum of this pallet"]
        pub type Event = runtime_types::pallet_transaction_payment::pallet::Event;
        pub mod events {
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "A transaction fee `actual_fee`, of which `tip` was added to the minimum inclusion fee,"]
            #[doc = "has been paid by `who`."]
            pub struct TransactionFeePaid {
                pub who: transaction_fee_paid::Who,
                pub actual_fee: transaction_fee_paid::ActualFee,
                pub tip: transaction_fee_paid::Tip,
            }
            pub mod transaction_fee_paid {
                use super::runtime_types;
                pub type Who = ::subxt::utils::AccountId32;
                pub type ActualFee = ::core::primitive::u128;
                pub type Tip = ::core::primitive::u128;
            }
            impl TransactionFeePaid {
                const PALLET_NAME: &'static str = "TransactionPayment";
                const EVENT_NAME: &'static str = "TransactionFeePaid";
            }
            impl ::subxt::events::DecodeAsEvent for TransactionFeePaid {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
        }
        pub mod storage {
            use super::root_mod;
            use super::runtime_types;
            pub struct StorageApi;
            impl StorageApi {
                pub fn next_fee_multiplier(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (),
                    next_fee_multiplier::Output,
                    ::subxt::utils::Yes,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "TransactionPayment",
                        "NextFeeMultiplier",
                        [
                            54u8, 78u8, 153u8, 36u8, 231u8, 148u8, 27u8, 187u8, 224u8, 89u8, 193u8,
                            138u8, 18u8, 92u8, 61u8, 225u8, 78u8, 186u8, 175u8, 214u8, 45u8, 237u8,
                            65u8, 225u8, 177u8, 110u8, 113u8, 22u8, 164u8, 172u8, 191u8, 241u8,
                        ],
                    )
                }
                pub fn storage_version(
                    &self,
                ) -> ::subxt::storage::StaticAddress<(), storage_version::Output, ::subxt::utils::Yes>
                {
                    ::subxt::storage::StaticAddress::new_static(
                        "TransactionPayment",
                        "StorageVersion",
                        [
                            102u8, 2u8, 115u8, 199u8, 149u8, 230u8, 163u8, 131u8, 198u8, 138u8,
                            203u8, 116u8, 26u8, 120u8, 43u8, 39u8, 234u8, 52u8, 229u8, 102u8,
                            194u8, 18u8, 44u8, 249u8, 84u8, 142u8, 217u8, 129u8, 80u8, 5u8, 194u8,
                            214u8,
                        ],
                    )
                }
                #[doc = " The `OnChargeTransaction` stores the withdrawn tx fee here."]
                #[doc = ""]
                #[doc = " Use `withdraw_txfee` and `remaining_txfee` to access from outside the crate."]
                pub fn tx_payment_credit(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (),
                    tx_payment_credit::Output,
                    ::subxt::utils::Yes,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "TransactionPayment",
                        "TxPaymentCredit",
                        [
                            200u8, 46u8, 84u8, 207u8, 2u8, 81u8, 201u8, 150u8, 218u8, 189u8, 138u8,
                            151u8, 91u8, 194u8, 144u8, 2u8, 28u8, 38u8, 88u8, 233u8, 242u8, 207u8,
                            20u8, 172u8, 99u8, 167u8, 57u8, 12u8, 121u8, 0u8, 162u8, 148u8,
                        ],
                    )
                }
            }
            pub mod next_fee_multiplier {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = runtime_types::sp_arithmetic::fixed_point::FixedU128;
            }
            pub mod storage_version {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = runtime_types::pallet_transaction_payment::Releases;
            }
            pub mod tx_payment_credit {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                }
                pub type Output = runtime_types::frame_support::traits::storage::NoDrop<
                    runtime_types::frame_support::traits::tokens::fungible::imbalance::Imbalance<
                        ::core::primitive::u128,
                    >,
                >;
            }
        }
        pub mod constants {
            use super::runtime_types;
            pub struct ConstantsApi;
            impl ConstantsApi {
                #[doc = " A fee multiplier for `Operational` extrinsics to compute \"virtual tip\" to boost their"]
                #[doc = " `priority`"]
                #[doc = ""]
                #[doc = " This value is multiplied by the `final_fee` to obtain a \"virtual tip\" that is later"]
                #[doc = " added to a tip component in regular `priority` calculations."]
                #[doc = " It means that a `Normal` transaction can front-run a similarly-sized `Operational`"]
                #[doc = " extrinsic (with no tip), by including a tip value greater than the virtual tip."]
                #[doc = ""]
                #[doc = " ```rust,ignore"]
                #[doc = " // For `Normal`"]
                #[doc = " let priority = priority_calc(tip);"]
                #[doc = ""]
                #[doc = " // For `Operational`"]
                #[doc = " let virtual_tip = (inclusion_fee + tip) * OperationalFeeMultiplier;"]
                #[doc = " let priority = priority_calc(tip + virtual_tip);"]
                #[doc = " ```"]
                #[doc = ""]
                #[doc = " Note that since we use `final_fee` the multiplier applies also to the regular `tip`"]
                #[doc = " sent with the transaction. So, not only does the transaction get a priority bump based"]
                #[doc = " on the `inclusion_fee`, but we also amplify the impact of tips applied to `Operational`"]
                #[doc = " transactions."]
                pub fn operational_fee_multiplier(
                    &self,
                ) -> ::subxt::constants::StaticAddress<::core::primitive::u8> {
                    ::subxt::constants::StaticAddress::new_static(
                        "TransactionPayment",
                        "OperationalFeeMultiplier",
                        [
                            141u8, 130u8, 11u8, 35u8, 226u8, 114u8, 92u8, 179u8, 168u8, 110u8,
                            28u8, 91u8, 221u8, 64u8, 4u8, 148u8, 201u8, 193u8, 185u8, 66u8, 226u8,
                            114u8, 97u8, 79u8, 62u8, 212u8, 202u8, 114u8, 237u8, 228u8, 183u8,
                            165u8,
                        ],
                    )
                }
            }
        }
    }
    pub mod content {
        use super::root_mod;
        use super::runtime_types;
        #[doc = "Errors returned by the content pallet."]
        pub type Error = runtime_types::pallet_content::pallet::Error;
        #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
        pub type Call = runtime_types::pallet_content::pallet::Call;
        pub mod calls {
            use super::root_mod;
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Publishes a new item and its initial revision."]
            #[doc = ""]
            #[doc = "The item id is derived from the signer, the supplied [`Nonce`], and"]
            #[doc = "[`Config::ItemIdNamespace`]. The call persists only ownership,"]
            #[doc = "revision, and flag metadata; graph edges and the payload reference are"]
            #[doc = "emitted in events for off-chain indexing."]
            pub struct PublishItem {
                pub nonce: publish_item::Nonce,
                pub parents: publish_item::Parents,
                pub flags: publish_item::Flags,
                pub links: publish_item::Links,
                pub mentions: publish_item::Mentions,
                pub ipfs_hash: publish_item::IpfsHash,
            }
            pub mod publish_item {
                use super::runtime_types;
                pub type Nonce = runtime_types::pallet_content::Nonce;
                pub type Parents = runtime_types::bounded_collections::bounded_vec::BoundedVec<
                    runtime_types::pallet_content::pallet::ItemId,
                >;
                pub type Flags = ::core::primitive::u8;
                pub type Links = runtime_types::bounded_collections::bounded_vec::BoundedVec<
                    runtime_types::pallet_content::pallet::ItemId,
                >;
                pub type Mentions = runtime_types::bounded_collections::bounded_vec::BoundedVec<
                    ::subxt::utils::AccountId32,
                >;
                pub type IpfsHash = runtime_types::pallet_content::pallet::IpfsHash;
            }
            impl PublishItem {
                const PALLET_NAME: &'static str = "Content";
                const CALL_NAME: &'static str = "publish_item";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for PublishItem {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Publishes a new revision for an existing item."]
            #[doc = ""]
            #[doc = "Only the current item owner can publish revisions, and only while the"]
            #[doc = "item is marked [`REVISIONABLE`] and not [`RETRACTED`]."]
            pub struct PublishRevision {
                pub item_id: publish_revision::ItemId,
                pub links: publish_revision::Links,
                pub mentions: publish_revision::Mentions,
                pub ipfs_hash: publish_revision::IpfsHash,
            }
            pub mod publish_revision {
                use super::runtime_types;
                pub type ItemId = runtime_types::pallet_content::pallet::ItemId;
                pub type Links = runtime_types::bounded_collections::bounded_vec::BoundedVec<
                    runtime_types::pallet_content::pallet::ItemId,
                >;
                pub type Mentions = runtime_types::bounded_collections::bounded_vec::BoundedVec<
                    ::subxt::utils::AccountId32,
                >;
                pub type IpfsHash = runtime_types::pallet_content::pallet::IpfsHash;
            }
            impl PublishRevision {
                const PALLET_NAME: &'static str = "Content";
                const CALL_NAME: &'static str = "publish_revision";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for PublishRevision {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Marks an item as retracted."]
            #[doc = ""]
            #[doc = "Only the owner can retract, and only while the item still has the"]
            #[doc = "[`RETRACTABLE`] permission bit set."]
            pub struct RetractItem {
                pub item_id: retract_item::ItemId,
            }
            pub mod retract_item {
                use super::runtime_types;
                pub type ItemId = runtime_types::pallet_content::pallet::ItemId;
            }
            impl RetractItem {
                const PALLET_NAME: &'static str = "Content";
                const CALL_NAME: &'static str = "retract_item";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for RetractItem {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Permanently disables future revisions for an item."]
            pub struct SetNotRevisionable {
                pub item_id: set_not_revisionable::ItemId,
            }
            pub mod set_not_revisionable {
                use super::runtime_types;
                pub type ItemId = runtime_types::pallet_content::pallet::ItemId;
            }
            impl SetNotRevisionable {
                const PALLET_NAME: &'static str = "Content";
                const CALL_NAME: &'static str = "set_not_revisionable";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for SetNotRevisionable {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Permanently disables future retraction for an item."]
            pub struct SetNotRetractable {
                pub item_id: set_not_retractable::ItemId,
            }
            pub mod set_not_retractable {
                use super::runtime_types;
                pub type ItemId = runtime_types::pallet_content::pallet::ItemId;
            }
            impl SetNotRetractable {
                const PALLET_NAME: &'static str = "Content";
                const CALL_NAME: &'static str = "set_not_retractable";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for SetNotRetractable {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            pub mod api {
                pub struct TransactionApi;
                impl TransactionApi {
                    #[doc = "Publishes a new item and its initial revision."]
                    #[doc = ""]
                    #[doc = "The item id is derived from the signer, the supplied [`Nonce`], and"]
                    #[doc = "[`Config::ItemIdNamespace`]. The call persists only ownership,"]
                    #[doc = "revision, and flag metadata; graph edges and the payload reference are"]
                    #[doc = "emitted in events for off-chain indexing."]
                    pub fn publish_item(
                        &self,
                        nonce: super::publish_item::Nonce,
                        parents: super::publish_item::Parents,
                        flags: super::publish_item::Flags,
                        links: super::publish_item::Links,
                        mentions: super::publish_item::Mentions,
                        ipfs_hash: super::publish_item::IpfsHash,
                    ) -> ::subxt::transactions::StaticPayload<super::PublishItem>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Content",
                            "publish_item",
                            super::PublishItem {
                                nonce,
                                parents,
                                flags,
                                links,
                                mentions,
                                ipfs_hash,
                            },
                            [
                                98u8, 55u8, 244u8, 208u8, 252u8, 15u8, 38u8, 238u8, 253u8, 198u8,
                                71u8, 227u8, 117u8, 20u8, 111u8, 17u8, 61u8, 25u8, 115u8, 3u8,
                                226u8, 217u8, 217u8, 112u8, 156u8, 99u8, 82u8, 25u8, 59u8, 13u8,
                                11u8, 126u8,
                            ],
                        )
                    }
                    #[doc = "Publishes a new revision for an existing item."]
                    #[doc = ""]
                    #[doc = "Only the current item owner can publish revisions, and only while the"]
                    #[doc = "item is marked [`REVISIONABLE`] and not [`RETRACTED`]."]
                    pub fn publish_revision(
                        &self,
                        item_id: super::publish_revision::ItemId,
                        links: super::publish_revision::Links,
                        mentions: super::publish_revision::Mentions,
                        ipfs_hash: super::publish_revision::IpfsHash,
                    ) -> ::subxt::transactions::StaticPayload<super::PublishRevision>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Content",
                            "publish_revision",
                            super::PublishRevision {
                                item_id,
                                links,
                                mentions,
                                ipfs_hash,
                            },
                            [
                                37u8, 244u8, 138u8, 22u8, 203u8, 42u8, 23u8, 185u8, 246u8, 166u8,
                                139u8, 169u8, 224u8, 189u8, 62u8, 62u8, 179u8, 86u8, 95u8, 49u8,
                                196u8, 173u8, 242u8, 59u8, 11u8, 223u8, 53u8, 219u8, 113u8, 147u8,
                                243u8, 69u8,
                            ],
                        )
                    }
                    #[doc = "Marks an item as retracted."]
                    #[doc = ""]
                    #[doc = "Only the owner can retract, and only while the item still has the"]
                    #[doc = "[`RETRACTABLE`] permission bit set."]
                    pub fn retract_item(
                        &self,
                        item_id: super::retract_item::ItemId,
                    ) -> ::subxt::transactions::StaticPayload<super::RetractItem>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Content",
                            "retract_item",
                            super::RetractItem { item_id },
                            [
                                93u8, 195u8, 37u8, 230u8, 168u8, 73u8, 76u8, 220u8, 7u8, 241u8,
                                82u8, 16u8, 160u8, 41u8, 251u8, 192u8, 220u8, 87u8, 159u8, 237u8,
                                110u8, 160u8, 221u8, 85u8, 129u8, 57u8, 73u8, 70u8, 42u8, 130u8,
                                149u8, 155u8,
                            ],
                        )
                    }
                    #[doc = "Permanently disables future revisions for an item."]
                    pub fn set_not_revisionable(
                        &self,
                        item_id: super::set_not_revisionable::ItemId,
                    ) -> ::subxt::transactions::StaticPayload<super::SetNotRevisionable>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Content",
                            "set_not_revisionable",
                            super::SetNotRevisionable { item_id },
                            [
                                37u8, 142u8, 218u8, 51u8, 254u8, 62u8, 166u8, 236u8, 55u8, 23u8,
                                18u8, 163u8, 95u8, 203u8, 208u8, 96u8, 91u8, 135u8, 221u8, 35u8,
                                211u8, 114u8, 1u8, 230u8, 230u8, 220u8, 65u8, 184u8, 131u8, 136u8,
                                174u8, 54u8,
                            ],
                        )
                    }
                    #[doc = "Permanently disables future retraction for an item."]
                    pub fn set_not_retractable(
                        &self,
                        item_id: super::set_not_retractable::ItemId,
                    ) -> ::subxt::transactions::StaticPayload<super::SetNotRetractable>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Content",
                            "set_not_retractable",
                            super::SetNotRetractable { item_id },
                            [
                                123u8, 93u8, 147u8, 205u8, 96u8, 42u8, 226u8, 181u8, 146u8, 199u8,
                                182u8, 150u8, 27u8, 192u8, 210u8, 74u8, 152u8, 73u8, 47u8, 100u8,
                                236u8, 145u8, 19u8, 102u8, 38u8, 81u8, 20u8, 134u8, 197u8, 79u8,
                                98u8, 187u8,
                            ],
                        )
                    }
                }
            }
        }
        #[doc = "The `Event` enum of this pallet"]
        pub type Event = runtime_types::pallet_content::pallet::Event;
        pub mod events {
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "A new item was created."]
            pub struct PublishItem {
                pub item_id: publish_item::ItemId,
                pub owner: publish_item::Owner,
                pub parents: publish_item::Parents,
                pub flags: publish_item::Flags,
            }
            pub mod publish_item {
                use super::runtime_types;
                pub type ItemId = runtime_types::pallet_content::pallet::ItemId;
                pub type Owner = ::subxt::utils::AccountId32;
                pub type Parents = runtime_types::bounded_collections::bounded_vec::BoundedVec<
                    runtime_types::pallet_content::pallet::ItemId,
                >;
                pub type Flags = ::core::primitive::u8;
            }
            impl PublishItem {
                const PALLET_NAME: &'static str = "Content";
                const EVENT_NAME: &'static str = "PublishItem";
            }
            impl ::subxt::events::DecodeAsEvent for PublishItem {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "A new revision was published for an item."]
            pub struct PublishRevision {
                pub item_id: publish_revision::ItemId,
                pub owner: publish_revision::Owner,
                pub revision_id: publish_revision::RevisionId,
                pub links: publish_revision::Links,
                pub mentions: publish_revision::Mentions,
                pub ipfs_hash: publish_revision::IpfsHash,
            }
            pub mod publish_revision {
                use super::runtime_types;
                pub type ItemId = runtime_types::pallet_content::pallet::ItemId;
                pub type Owner = ::subxt::utils::AccountId32;
                pub type RevisionId = ::core::primitive::u32;
                pub type Links = runtime_types::bounded_collections::bounded_vec::BoundedVec<
                    runtime_types::pallet_content::pallet::ItemId,
                >;
                pub type Mentions = runtime_types::bounded_collections::bounded_vec::BoundedVec<
                    ::subxt::utils::AccountId32,
                >;
                pub type IpfsHash = runtime_types::pallet_content::pallet::IpfsHash;
            }
            impl PublishRevision {
                const PALLET_NAME: &'static str = "Content";
                const EVENT_NAME: &'static str = "PublishRevision";
            }
            impl ::subxt::events::DecodeAsEvent for PublishRevision {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "An item was marked as retracted."]
            pub struct RetractItem {
                pub item_id: retract_item::ItemId,
                pub owner: retract_item::Owner,
            }
            pub mod retract_item {
                use super::runtime_types;
                pub type ItemId = runtime_types::pallet_content::pallet::ItemId;
                pub type Owner = ::subxt::utils::AccountId32;
            }
            impl RetractItem {
                const PALLET_NAME: &'static str = "Content";
                const EVENT_NAME: &'static str = "RetractItem";
            }
            impl ::subxt::events::DecodeAsEvent for RetractItem {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Revision publishing was permanently disabled for an item."]
            pub struct SetNotRevsionable {
                pub item_id: set_not_revsionable::ItemId,
                pub owner: set_not_revsionable::Owner,
            }
            pub mod set_not_revsionable {
                use super::runtime_types;
                pub type ItemId = runtime_types::pallet_content::pallet::ItemId;
                pub type Owner = ::subxt::utils::AccountId32;
            }
            impl SetNotRevsionable {
                const PALLET_NAME: &'static str = "Content";
                const EVENT_NAME: &'static str = "SetNotRevsionable";
            }
            impl ::subxt::events::DecodeAsEvent for SetNotRevsionable {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Retraction was permanently disabled for an item."]
            pub struct SetNotRetractable {
                pub item_id: set_not_retractable::ItemId,
                pub owner: set_not_retractable::Owner,
            }
            pub mod set_not_retractable {
                use super::runtime_types;
                pub type ItemId = runtime_types::pallet_content::pallet::ItemId;
                pub type Owner = ::subxt::utils::AccountId32;
            }
            impl SetNotRetractable {
                const PALLET_NAME: &'static str = "Content";
                const EVENT_NAME: &'static str = "SetNotRetractable";
            }
            impl ::subxt::events::DecodeAsEvent for SetNotRetractable {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
        }
        pub mod storage {
            use super::root_mod;
            use super::runtime_types;
            pub struct StorageApi;
            impl StorageApi {
                #[doc = " Canonical item metadata keyed by deterministic [`ItemId`]."]
                pub fn item_state(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (item_state::input::Param0,),
                    item_state::Output,
                    ::subxt::utils::Maybe,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "Content",
                        "ItemState",
                        [
                            210u8, 114u8, 33u8, 44u8, 133u8, 254u8, 83u8, 162u8, 168u8, 225u8,
                            212u8, 67u8, 60u8, 176u8, 240u8, 231u8, 93u8, 59u8, 157u8, 138u8, 19u8,
                            241u8, 112u8, 75u8, 153u8, 45u8, 29u8, 243u8, 213u8, 110u8, 211u8,
                            110u8,
                        ],
                    )
                }
            }
            pub mod item_state {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                    pub type Param0 = runtime_types::pallet_content::pallet::ItemId;
                }
                pub type Output =
                    runtime_types::pallet_content::pallet::Item<::subxt::utils::AccountId32>;
            }
        }
    }
    pub mod account_content {
        use super::root_mod;
        use super::runtime_types;
        #[doc = "Errors returned by the account-content pallet."]
        pub type Error = runtime_types::pallet_account_content::pallet::Error;
        #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
        pub type Call = runtime_types::pallet_account_content::pallet::Call;
        pub mod calls {
            use super::root_mod;
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Adds a content item to the caller's ordered list."]
            #[doc = ""]
            #[doc = "The referenced item must exist in `pallet-content`, must not be"]
            #[doc = "retracted, and must currently be owned by the caller."]
            pub struct AddItem {
                pub item_id: add_item::ItemId,
            }
            pub mod add_item {
                use super::runtime_types;
                pub type ItemId = runtime_types::pallet_content::pallet::ItemId;
            }
            impl AddItem {
                const PALLET_NAME: &'static str = "AccountContent";
                const CALL_NAME: &'static str = "add_item";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for AddItem {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Removes a content item from the caller's ordered list."]
            #[doc = ""]
            #[doc = "Removal uses swap-with-last semantics so membership checks and"]
            #[doc = "deletions stay O(1)."]
            pub struct RemoveItem {
                pub item_id: remove_item::ItemId,
            }
            pub mod remove_item {
                use super::runtime_types;
                pub type ItemId = runtime_types::pallet_content::pallet::ItemId;
            }
            impl RemoveItem {
                const PALLET_NAME: &'static str = "AccountContent";
                const CALL_NAME: &'static str = "remove_item";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for RemoveItem {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            pub mod api {
                pub struct TransactionApi;
                impl TransactionApi {
                    #[doc = "Adds a content item to the caller's ordered list."]
                    #[doc = ""]
                    #[doc = "The referenced item must exist in `pallet-content`, must not be"]
                    #[doc = "retracted, and must currently be owned by the caller."]
                    pub fn add_item(
                        &self,
                        item_id: super::add_item::ItemId,
                    ) -> ::subxt::transactions::StaticPayload<super::AddItem> {
                        ::subxt::transactions::StaticPayload::new_static(
                            "AccountContent",
                            "add_item",
                            super::AddItem { item_id },
                            [
                                59u8, 102u8, 18u8, 164u8, 144u8, 124u8, 247u8, 3u8, 135u8, 56u8,
                                175u8, 190u8, 133u8, 161u8, 132u8, 242u8, 80u8, 70u8, 245u8, 67u8,
                                94u8, 155u8, 222u8, 28u8, 96u8, 222u8, 84u8, 207u8, 17u8, 146u8,
                                227u8, 107u8,
                            ],
                        )
                    }
                    #[doc = "Removes a content item from the caller's ordered list."]
                    #[doc = ""]
                    #[doc = "Removal uses swap-with-last semantics so membership checks and"]
                    #[doc = "deletions stay O(1)."]
                    pub fn remove_item(
                        &self,
                        item_id: super::remove_item::ItemId,
                    ) -> ::subxt::transactions::StaticPayload<super::RemoveItem>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "AccountContent",
                            "remove_item",
                            super::RemoveItem { item_id },
                            [
                                37u8, 99u8, 101u8, 133u8, 225u8, 233u8, 134u8, 50u8, 79u8, 119u8,
                                246u8, 230u8, 8u8, 201u8, 229u8, 22u8, 178u8, 231u8, 75u8, 232u8,
                                100u8, 104u8, 9u8, 129u8, 217u8, 227u8, 37u8, 22u8, 182u8, 1u8,
                                198u8, 212u8,
                            ],
                        )
                    }
                }
            }
        }
        #[doc = "The `Event` enum of this pallet"]
        pub type Event = runtime_types::pallet_account_content::pallet::Event;
        pub mod events {
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "An item was added to an account list."]
            pub struct AddItem {
                pub account: add_item::Account,
                pub item_id: add_item::ItemId,
            }
            pub mod add_item {
                use super::runtime_types;
                pub type Account = ::subxt::utils::AccountId32;
                pub type ItemId = runtime_types::pallet_content::pallet::ItemId;
            }
            impl AddItem {
                const PALLET_NAME: &'static str = "AccountContent";
                const EVENT_NAME: &'static str = "AddItem";
            }
            impl ::subxt::events::DecodeAsEvent for AddItem {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "An item was removed from an account list."]
            pub struct RemoveItem {
                pub account: remove_item::Account,
                pub item_id: remove_item::ItemId,
            }
            pub mod remove_item {
                use super::runtime_types;
                pub type Account = ::subxt::utils::AccountId32;
                pub type ItemId = runtime_types::pallet_content::pallet::ItemId;
            }
            impl RemoveItem {
                const PALLET_NAME: &'static str = "AccountContent";
                const EVENT_NAME: &'static str = "RemoveItem";
            }
            impl ::subxt::events::DecodeAsEvent for RemoveItem {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
        }
        pub mod storage {
            use super::root_mod;
            use super::runtime_types;
            pub struct StorageApi;
            impl StorageApi {
                #[doc = " Ordered content item ids keyed by account."]
                pub fn account_item_ids(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (account_item_ids::input::Param0,),
                    account_item_ids::Output,
                    ::subxt::utils::Maybe,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "AccountContent",
                        "AccountItemIds",
                        [
                            177u8, 171u8, 57u8, 83u8, 249u8, 193u8, 189u8, 204u8, 155u8, 99u8,
                            99u8, 40u8, 88u8, 214u8, 172u8, 184u8, 45u8, 120u8, 206u8, 97u8, 173u8,
                            162u8, 86u8, 83u8, 124u8, 3u8, 212u8, 79u8, 73u8, 233u8, 106u8, 139u8,
                        ],
                    )
                }
                #[doc = " Reverse lookup from `(account, item_id)` to `index + 1` in [`AccountItemIds`]."]
                pub fn account_item_id_index(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (
                        account_item_id_index::input::Param0,
                        account_item_id_index::input::Param1,
                    ),
                    account_item_id_index::Output,
                    ::subxt::utils::Maybe,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "AccountContent",
                        "AccountItemIdIndex",
                        [
                            192u8, 31u8, 104u8, 105u8, 139u8, 245u8, 198u8, 66u8, 42u8, 147u8,
                            102u8, 160u8, 171u8, 178u8, 217u8, 243u8, 55u8, 227u8, 70u8, 124u8,
                            234u8, 92u8, 126u8, 157u8, 218u8, 199u8, 203u8, 83u8, 84u8, 103u8,
                            194u8, 16u8,
                        ],
                    )
                }
            }
            pub mod account_item_ids {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                    pub type Param0 = ::subxt::utils::AccountId32;
                }
                pub type Output = runtime_types::bounded_collections::bounded_vec::BoundedVec<
                    runtime_types::pallet_content::pallet::ItemId,
                >;
            }
            pub mod account_item_id_index {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                    pub type Param0 = ::subxt::utils::AccountId32;
                    pub type Param1 = runtime_types::pallet_content::pallet::ItemId;
                }
                pub type Output = ::core::primitive::u32;
            }
        }
    }
    pub mod account_profile {
        use super::root_mod;
        use super::runtime_types;
        #[doc = "Errors returned by the account-profile pallet."]
        pub type Error = runtime_types::pallet_account_profile::pallet::Error;
        #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
        pub type Call = runtime_types::pallet_account_profile::pallet::Call;
        pub mod calls {
            use super::root_mod;
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Sets or overwrites the caller's profile pointer."]
            #[doc = ""]
            #[doc = "The referenced item must exist in `pallet-content`, must not be"]
            #[doc = "retracted, and must currently be owned by the caller."]
            pub struct SetProfile {
                pub item_id: set_profile::ItemId,
            }
            pub mod set_profile {
                use super::runtime_types;
                pub type ItemId = runtime_types::pallet_content::pallet::ItemId;
            }
            impl SetProfile {
                const PALLET_NAME: &'static str = "AccountProfile";
                const CALL_NAME: &'static str = "set_profile";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for SetProfile {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            pub mod api {
                pub struct TransactionApi;
                impl TransactionApi {
                    #[doc = "Sets or overwrites the caller's profile pointer."]
                    #[doc = ""]
                    #[doc = "The referenced item must exist in `pallet-content`, must not be"]
                    #[doc = "retracted, and must currently be owned by the caller."]
                    pub fn set_profile(
                        &self,
                        item_id: super::set_profile::ItemId,
                    ) -> ::subxt::transactions::StaticPayload<super::SetProfile>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "AccountProfile",
                            "set_profile",
                            super::SetProfile { item_id },
                            [
                                53u8, 25u8, 84u8, 40u8, 183u8, 170u8, 36u8, 36u8, 212u8, 18u8, 7u8,
                                6u8, 114u8, 81u8, 93u8, 47u8, 185u8, 87u8, 33u8, 146u8, 87u8, 49u8,
                                7u8, 1u8, 176u8, 218u8, 97u8, 48u8, 63u8, 117u8, 165u8, 122u8,
                            ],
                        )
                    }
                }
            }
        }
        #[doc = "The `Event` enum of this pallet"]
        pub type Event = runtime_types::pallet_account_profile::pallet::Event;
        pub mod events {
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "A profile pointer was set or replaced."]
            pub struct ProfileSet {
                pub account: profile_set::Account,
                pub item_id: profile_set::ItemId,
            }
            pub mod profile_set {
                use super::runtime_types;
                pub type Account = ::subxt::utils::AccountId32;
                pub type ItemId = runtime_types::pallet_content::pallet::ItemId;
            }
            impl ProfileSet {
                const PALLET_NAME: &'static str = "AccountProfile";
                const EVENT_NAME: &'static str = "ProfileSet";
            }
            impl ::subxt::events::DecodeAsEvent for ProfileSet {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
        }
        pub mod storage {
            use super::root_mod;
            use super::runtime_types;
            pub struct StorageApi;
            impl StorageApi {
                #[doc = " Profile content item currently associated with each account."]
                pub fn account_profile(
                    &self,
                ) -> ::subxt::storage::StaticAddress<
                    (account_profile::input::Param0,),
                    account_profile::Output,
                    ::subxt::utils::Maybe,
                > {
                    ::subxt::storage::StaticAddress::new_static(
                        "AccountProfile",
                        "AccountProfile",
                        [
                            248u8, 161u8, 88u8, 60u8, 91u8, 148u8, 150u8, 234u8, 37u8, 94u8, 169u8,
                            53u8, 132u8, 168u8, 109u8, 29u8, 208u8, 222u8, 12u8, 223u8, 59u8,
                            248u8, 146u8, 74u8, 188u8, 192u8, 241u8, 25u8, 181u8, 31u8, 50u8,
                            132u8,
                        ],
                    )
                }
            }
            pub mod account_profile {
                use super::root_mod;
                use super::runtime_types;
                pub mod input {
                    use super::runtime_types;
                    pub type Param0 = ::subxt::utils::AccountId32;
                }
                pub type Output = runtime_types::pallet_content::pallet::ItemId;
            }
        }
    }
    pub mod content_reactions {
        use super::root_mod;
        use super::runtime_types;
        #[doc = "Errors returned by the content-reactions pallet."]
        pub type Error = runtime_types::pallet_content_reactions::pallet::Error;
        #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
        pub type Call = runtime_types::pallet_content_reactions::pallet::Call;
        pub mod calls {
            use super::root_mod;
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Sets the caller's full reaction set for a specific item revision,"]
            #[doc = "replacing any prior reactions."]
            #[doc = ""]
            #[doc = "The entire reaction set is emitted in a single `SetReactions` event."]
            #[doc = "Duplicates within the provided set are rejected."]
            pub struct SetReactions {
                pub item_id: set_reactions::ItemId,
                pub revision_id: set_reactions::RevisionId,
                pub reactions: set_reactions::Reactions,
            }
            pub mod set_reactions {
                use super::runtime_types;
                pub type ItemId = runtime_types::pallet_content::pallet::ItemId;
                pub type RevisionId = ::core::primitive::u32;
                pub type Reactions = runtime_types::bounded_collections::bounded_vec::BoundedVec<
                    runtime_types::pallet_content_reactions::pallet::Emoji,
                >;
            }
            impl SetReactions {
                const PALLET_NAME: &'static str = "ContentReactions";
                const CALL_NAME: &'static str = "set_reactions";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for SetReactions {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            pub mod api {
                pub struct TransactionApi;
                impl TransactionApi {
                    #[doc = "Sets the caller's full reaction set for a specific item revision,"]
                    #[doc = "replacing any prior reactions."]
                    #[doc = ""]
                    #[doc = "The entire reaction set is emitted in a single `SetReactions` event."]
                    #[doc = "Duplicates within the provided set are rejected."]
                    pub fn set_reactions(
                        &self,
                        item_id: super::set_reactions::ItemId,
                        revision_id: super::set_reactions::RevisionId,
                        reactions: super::set_reactions::Reactions,
                    ) -> ::subxt::transactions::StaticPayload<super::SetReactions>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "ContentReactions",
                            "set_reactions",
                            super::SetReactions {
                                item_id,
                                revision_id,
                                reactions,
                            },
                            [
                                48u8, 125u8, 91u8, 239u8, 153u8, 94u8, 97u8, 71u8, 171u8, 78u8,
                                211u8, 167u8, 101u8, 67u8, 1u8, 205u8, 16u8, 135u8, 139u8, 140u8,
                                70u8, 237u8, 243u8, 1u8, 114u8, 131u8, 3u8, 83u8, 141u8, 55u8,
                                14u8, 56u8,
                            ],
                        )
                    }
                }
            }
        }
        #[doc = "The `Event` enum of this pallet"]
        pub type Event = runtime_types::pallet_content_reactions::pallet::Event;
        pub mod events {
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "The caller's reaction set for a specific item revision was set."]
            pub struct SetReactions {
                pub item_id: set_reactions::ItemId,
                pub revision_id: set_reactions::RevisionId,
                pub item_owner: set_reactions::ItemOwner,
                pub reactor: set_reactions::Reactor,
                pub reactions: set_reactions::Reactions,
            }
            pub mod set_reactions {
                use super::runtime_types;
                pub type ItemId = runtime_types::pallet_content::pallet::ItemId;
                pub type RevisionId = ::core::primitive::u32;
                pub type ItemOwner = ::subxt::utils::AccountId32;
                pub type Reactor = ::subxt::utils::AccountId32;
                pub type Reactions = runtime_types::bounded_collections::bounded_vec::BoundedVec<
                    runtime_types::pallet_content_reactions::pallet::Emoji,
                >;
            }
            impl SetReactions {
                const PALLET_NAME: &'static str = "ContentReactions";
                const EVENT_NAME: &'static str = "SetReactions";
            }
            impl ::subxt::events::DecodeAsEvent for SetReactions {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
        }
    }
    pub mod utility {
        use super::root_mod;
        use super::runtime_types;
        #[doc = "The `Error` enum of this pallet."]
        pub type Error = runtime_types::pallet_utility::pallet::Error;
        #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
        pub type Call = runtime_types::pallet_utility::pallet::Call;
        pub mod calls {
            use super::root_mod;
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Send a batch of dispatch calls."]
            #[doc = ""]
            #[doc = "May be called from any origin except `None`."]
            #[doc = ""]
            #[doc = "- `calls`: The calls to be dispatched from the same origin. The number of call must not"]
            #[doc = "  exceed the constant: `batched_calls_limit` (available in constant metadata)."]
            #[doc = ""]
            #[doc = "If origin is root then the calls are dispatched without checking origin filter. (This"]
            #[doc = "includes bypassing `frame_system::Config::BaseCallFilter`)."]
            #[doc = ""]
            #[doc = "## Complexity"]
            #[doc = "- O(C) where C is the number of calls to be batched."]
            #[doc = ""]
            #[doc = "This will return `Ok` in all circumstances. To determine the success of the batch, an"]
            #[doc = "event is deposited. If a call failed and the batch was interrupted, then the"]
            #[doc = "`BatchInterrupted` event is deposited, along with the number of successful calls made"]
            #[doc = "and the error of the failed call. If all were successful, then the `BatchCompleted`"]
            #[doc = "event is deposited."]
            pub struct Batch {
                pub calls: batch::Calls,
            }
            pub mod batch {
                use super::runtime_types;
                pub type Calls =
                    ::subxt::alloc::vec::Vec<runtime_types::acuity_runtime::RuntimeCall>;
            }
            impl Batch {
                const PALLET_NAME: &'static str = "Utility";
                const CALL_NAME: &'static str = "batch";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for Batch {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Send a call through an indexed pseudonym of the sender."]
            #[doc = ""]
            #[doc = "Filter from origin are passed along. The call will be dispatched with an origin which"]
            #[doc = "use the same filter as the origin of this call."]
            #[doc = ""]
            #[doc = "NOTE: If you need to ensure that any account-based filtering is not honored (i.e."]
            #[doc = "because you expect `proxy` to have been used prior in the call stack and you do not want"]
            #[doc = "the call restrictions to apply to any sub-accounts), then use `as_multi_threshold_1`"]
            #[doc = "in the Multisig pallet instead."]
            #[doc = ""]
            #[doc = "NOTE: Prior to version *12, this was called `as_limited_sub`."]
            #[doc = ""]
            #[doc = "The dispatch origin for this call must be _Signed_."]
            pub struct AsDerivative {
                pub index: as_derivative::Index,
                pub call: ::subxt::alloc::boxed::Box<as_derivative::Call>,
            }
            pub mod as_derivative {
                use super::runtime_types;
                pub type Index = ::core::primitive::u16;
                pub type Call = runtime_types::acuity_runtime::RuntimeCall;
            }
            impl AsDerivative {
                const PALLET_NAME: &'static str = "Utility";
                const CALL_NAME: &'static str = "as_derivative";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for AsDerivative {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Send a batch of dispatch calls and atomically execute them."]
            #[doc = "The whole transaction will rollback and fail if any of the calls failed."]
            #[doc = ""]
            #[doc = "May be called from any origin except `None`."]
            #[doc = ""]
            #[doc = "- `calls`: The calls to be dispatched from the same origin. The number of call must not"]
            #[doc = "  exceed the constant: `batched_calls_limit` (available in constant metadata)."]
            #[doc = ""]
            #[doc = "If origin is root then the calls are dispatched without checking origin filter. (This"]
            #[doc = "includes bypassing `frame_system::Config::BaseCallFilter`)."]
            #[doc = ""]
            #[doc = "## Complexity"]
            #[doc = "- O(C) where C is the number of calls to be batched."]
            pub struct BatchAll {
                pub calls: batch_all::Calls,
            }
            pub mod batch_all {
                use super::runtime_types;
                pub type Calls =
                    ::subxt::alloc::vec::Vec<runtime_types::acuity_runtime::RuntimeCall>;
            }
            impl BatchAll {
                const PALLET_NAME: &'static str = "Utility";
                const CALL_NAME: &'static str = "batch_all";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for BatchAll {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Dispatches a function call with a provided origin."]
            #[doc = ""]
            #[doc = "The dispatch origin for this call must be _Root_."]
            #[doc = ""]
            #[doc = "## Complexity"]
            #[doc = "- O(1)."]
            pub struct DispatchAs {
                pub as_origin: ::subxt::alloc::boxed::Box<dispatch_as::AsOrigin>,
                pub call: ::subxt::alloc::boxed::Box<dispatch_as::Call>,
            }
            pub mod dispatch_as {
                use super::runtime_types;
                pub type AsOrigin = runtime_types::acuity_runtime::OriginCaller;
                pub type Call = runtime_types::acuity_runtime::RuntimeCall;
            }
            impl DispatchAs {
                const PALLET_NAME: &'static str = "Utility";
                const CALL_NAME: &'static str = "dispatch_as";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for DispatchAs {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Send a batch of dispatch calls."]
            #[doc = "Unlike `batch`, it allows errors and won't interrupt."]
            #[doc = ""]
            #[doc = "May be called from any origin except `None`."]
            #[doc = ""]
            #[doc = "- `calls`: The calls to be dispatched from the same origin. The number of call must not"]
            #[doc = "  exceed the constant: `batched_calls_limit` (available in constant metadata)."]
            #[doc = ""]
            #[doc = "If origin is root then the calls are dispatch without checking origin filter. (This"]
            #[doc = "includes bypassing `frame_system::Config::BaseCallFilter`)."]
            #[doc = ""]
            #[doc = "## Complexity"]
            #[doc = "- O(C) where C is the number of calls to be batched."]
            pub struct ForceBatch {
                pub calls: force_batch::Calls,
            }
            pub mod force_batch {
                use super::runtime_types;
                pub type Calls =
                    ::subxt::alloc::vec::Vec<runtime_types::acuity_runtime::RuntimeCall>;
            }
            impl ForceBatch {
                const PALLET_NAME: &'static str = "Utility";
                const CALL_NAME: &'static str = "force_batch";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for ForceBatch {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Dispatch a function call with a specified weight."]
            #[doc = ""]
            #[doc = "This function does not check the weight of the call, and instead allows the"]
            #[doc = "Root origin to specify the weight of the call."]
            #[doc = ""]
            #[doc = "The dispatch origin for this call must be _Root_."]
            pub struct WithWeight {
                pub call: ::subxt::alloc::boxed::Box<with_weight::Call>,
                pub weight: with_weight::Weight,
            }
            pub mod with_weight {
                use super::runtime_types;
                pub type Call = runtime_types::acuity_runtime::RuntimeCall;
                pub type Weight = runtime_types::sp_weights::weight_v2::Weight;
            }
            impl WithWeight {
                const PALLET_NAME: &'static str = "Utility";
                const CALL_NAME: &'static str = "with_weight";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for WithWeight {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Dispatch a fallback call in the event the main call fails to execute."]
            #[doc = "May be called from any origin except `None`."]
            #[doc = ""]
            #[doc = "This function first attempts to dispatch the `main` call."]
            #[doc = "If the `main` call fails, the `fallback` is attemted."]
            #[doc = "if the fallback is successfully dispatched, the weights of both calls"]
            #[doc = "are accumulated and an event containing the main call error is deposited."]
            #[doc = ""]
            #[doc = "In the event of a fallback failure the whole call fails"]
            #[doc = "with the weights returned."]
            #[doc = ""]
            #[doc = "- `main`: The main call to be dispatched. This is the primary action to execute."]
            #[doc = "- `fallback`: The fallback call to be dispatched in case the `main` call fails."]
            #[doc = ""]
            #[doc = "## Dispatch Logic"]
            #[doc = "- If the origin is `root`, both the main and fallback calls are executed without"]
            #[doc = "  applying any origin filters."]
            #[doc = "- If the origin is not `root`, the origin filter is applied to both the `main` and"]
            #[doc = "  `fallback` calls."]
            #[doc = ""]
            #[doc = "## Use Case"]
            #[doc = "- Some use cases might involve submitting a `batch` type call in either main, fallback"]
            #[doc = "  or both."]
            pub struct IfElse {
                pub main: ::subxt::alloc::boxed::Box<if_else::Main>,
                pub fallback: ::subxt::alloc::boxed::Box<if_else::Fallback>,
            }
            pub mod if_else {
                use super::runtime_types;
                pub type Main = runtime_types::acuity_runtime::RuntimeCall;
                pub type Fallback = runtime_types::acuity_runtime::RuntimeCall;
            }
            impl IfElse {
                const PALLET_NAME: &'static str = "Utility";
                const CALL_NAME: &'static str = "if_else";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for IfElse {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Dispatches a function call with a provided origin."]
            #[doc = ""]
            #[doc = "Almost the same as [`Pallet::dispatch_as`] but forwards any error of the inner call."]
            #[doc = ""]
            #[doc = "The dispatch origin for this call must be _Root_."]
            pub struct DispatchAsFallible {
                pub as_origin: ::subxt::alloc::boxed::Box<dispatch_as_fallible::AsOrigin>,
                pub call: ::subxt::alloc::boxed::Box<dispatch_as_fallible::Call>,
            }
            pub mod dispatch_as_fallible {
                use super::runtime_types;
                pub type AsOrigin = runtime_types::acuity_runtime::OriginCaller;
                pub type Call = runtime_types::acuity_runtime::RuntimeCall;
            }
            impl DispatchAsFallible {
                const PALLET_NAME: &'static str = "Utility";
                const CALL_NAME: &'static str = "dispatch_as_fallible";
            }
            impl ::subxt::extrinsics::DecodeAsExtrinsic for DispatchAsFallible {
                fn is_extrinsic(pallet_name: &str, call_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && call_name == Self::CALL_NAME
                }
            }
            pub mod api {
                pub struct TransactionApi;
                impl TransactionApi {
                    #[doc = "Send a batch of dispatch calls."]
                    #[doc = ""]
                    #[doc = "May be called from any origin except `None`."]
                    #[doc = ""]
                    #[doc = "- `calls`: The calls to be dispatched from the same origin. The number of call must not"]
                    #[doc = "  exceed the constant: `batched_calls_limit` (available in constant metadata)."]
                    #[doc = ""]
                    #[doc = "If origin is root then the calls are dispatched without checking origin filter. (This"]
                    #[doc = "includes bypassing `frame_system::Config::BaseCallFilter`)."]
                    #[doc = ""]
                    #[doc = "## Complexity"]
                    #[doc = "- O(C) where C is the number of calls to be batched."]
                    #[doc = ""]
                    #[doc = "This will return `Ok` in all circumstances. To determine the success of the batch, an"]
                    #[doc = "event is deposited. If a call failed and the batch was interrupted, then the"]
                    #[doc = "`BatchInterrupted` event is deposited, along with the number of successful calls made"]
                    #[doc = "and the error of the failed call. If all were successful, then the `BatchCompleted`"]
                    #[doc = "event is deposited."]
                    pub fn batch(
                        &self,
                        calls: super::batch::Calls,
                    ) -> ::subxt::transactions::StaticPayload<super::Batch> {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Utility",
                            "batch",
                            super::Batch { calls },
                            [
                                50u8, 212u8, 166u8, 222u8, 118u8, 135u8, 100u8, 146u8, 165u8,
                                135u8, 195u8, 99u8, 151u8, 161u8, 102u8, 61u8, 166u8, 169u8, 56u8,
                                120u8, 248u8, 200u8, 105u8, 51u8, 74u8, 111u8, 181u8, 76u8, 93u8,
                                100u8, 161u8, 176u8,
                            ],
                        )
                    }
                    #[doc = "Send a call through an indexed pseudonym of the sender."]
                    #[doc = ""]
                    #[doc = "Filter from origin are passed along. The call will be dispatched with an origin which"]
                    #[doc = "use the same filter as the origin of this call."]
                    #[doc = ""]
                    #[doc = "NOTE: If you need to ensure that any account-based filtering is not honored (i.e."]
                    #[doc = "because you expect `proxy` to have been used prior in the call stack and you do not want"]
                    #[doc = "the call restrictions to apply to any sub-accounts), then use `as_multi_threshold_1`"]
                    #[doc = "in the Multisig pallet instead."]
                    #[doc = ""]
                    #[doc = "NOTE: Prior to version *12, this was called `as_limited_sub`."]
                    #[doc = ""]
                    #[doc = "The dispatch origin for this call must be _Signed_."]
                    pub fn as_derivative(
                        &self,
                        index: super::as_derivative::Index,
                        call: super::as_derivative::Call,
                    ) -> ::subxt::transactions::StaticPayload<super::AsDerivative>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Utility",
                            "as_derivative",
                            super::AsDerivative {
                                index,
                                call: ::subxt::alloc::boxed::Box::new(call),
                            },
                            [
                                48u8, 6u8, 35u8, 145u8, 92u8, 110u8, 202u8, 254u8, 133u8, 50u8,
                                190u8, 166u8, 103u8, 203u8, 252u8, 43u8, 210u8, 42u8, 38u8, 88u8,
                                25u8, 72u8, 13u8, 42u8, 211u8, 77u8, 193u8, 112u8, 176u8, 147u8,
                                118u8, 29u8,
                            ],
                        )
                    }
                    #[doc = "Send a batch of dispatch calls and atomically execute them."]
                    #[doc = "The whole transaction will rollback and fail if any of the calls failed."]
                    #[doc = ""]
                    #[doc = "May be called from any origin except `None`."]
                    #[doc = ""]
                    #[doc = "- `calls`: The calls to be dispatched from the same origin. The number of call must not"]
                    #[doc = "  exceed the constant: `batched_calls_limit` (available in constant metadata)."]
                    #[doc = ""]
                    #[doc = "If origin is root then the calls are dispatched without checking origin filter. (This"]
                    #[doc = "includes bypassing `frame_system::Config::BaseCallFilter`)."]
                    #[doc = ""]
                    #[doc = "## Complexity"]
                    #[doc = "- O(C) where C is the number of calls to be batched."]
                    pub fn batch_all(
                        &self,
                        calls: super::batch_all::Calls,
                    ) -> ::subxt::transactions::StaticPayload<super::BatchAll> {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Utility",
                            "batch_all",
                            super::BatchAll { calls },
                            [
                                12u8, 57u8, 50u8, 160u8, 172u8, 192u8, 112u8, 42u8, 14u8, 58u8,
                                44u8, 193u8, 85u8, 32u8, 109u8, 79u8, 163u8, 111u8, 21u8, 29u8,
                                78u8, 88u8, 231u8, 20u8, 93u8, 49u8, 43u8, 182u8, 187u8, 147u8,
                                51u8, 11u8,
                            ],
                        )
                    }
                    #[doc = "Dispatches a function call with a provided origin."]
                    #[doc = ""]
                    #[doc = "The dispatch origin for this call must be _Root_."]
                    #[doc = ""]
                    #[doc = "## Complexity"]
                    #[doc = "- O(1)."]
                    pub fn dispatch_as(
                        &self,
                        as_origin: super::dispatch_as::AsOrigin,
                        call: super::dispatch_as::Call,
                    ) -> ::subxt::transactions::StaticPayload<super::DispatchAs>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Utility",
                            "dispatch_as",
                            super::DispatchAs {
                                as_origin: ::subxt::alloc::boxed::Box::new(as_origin),
                                call: ::subxt::alloc::boxed::Box::new(call),
                            },
                            [
                                54u8, 11u8, 106u8, 143u8, 91u8, 101u8, 45u8, 51u8, 127u8, 205u8,
                                238u8, 244u8, 224u8, 233u8, 219u8, 213u8, 12u8, 188u8, 33u8, 157u8,
                                220u8, 163u8, 145u8, 183u8, 114u8, 47u8, 52u8, 230u8, 154u8, 218u8,
                                15u8, 9u8,
                            ],
                        )
                    }
                    #[doc = "Send a batch of dispatch calls."]
                    #[doc = "Unlike `batch`, it allows errors and won't interrupt."]
                    #[doc = ""]
                    #[doc = "May be called from any origin except `None`."]
                    #[doc = ""]
                    #[doc = "- `calls`: The calls to be dispatched from the same origin. The number of call must not"]
                    #[doc = "  exceed the constant: `batched_calls_limit` (available in constant metadata)."]
                    #[doc = ""]
                    #[doc = "If origin is root then the calls are dispatch without checking origin filter. (This"]
                    #[doc = "includes bypassing `frame_system::Config::BaseCallFilter`)."]
                    #[doc = ""]
                    #[doc = "## Complexity"]
                    #[doc = "- O(C) where C is the number of calls to be batched."]
                    pub fn force_batch(
                        &self,
                        calls: super::force_batch::Calls,
                    ) -> ::subxt::transactions::StaticPayload<super::ForceBatch>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Utility",
                            "force_batch",
                            super::ForceBatch { calls },
                            [
                                119u8, 204u8, 230u8, 66u8, 178u8, 192u8, 128u8, 119u8, 131u8,
                                245u8, 234u8, 147u8, 0u8, 106u8, 33u8, 181u8, 251u8, 184u8, 56u8,
                                55u8, 219u8, 178u8, 180u8, 152u8, 59u8, 158u8, 214u8, 88u8, 246u8,
                                48u8, 154u8, 253u8,
                            ],
                        )
                    }
                    #[doc = "Dispatch a function call with a specified weight."]
                    #[doc = ""]
                    #[doc = "This function does not check the weight of the call, and instead allows the"]
                    #[doc = "Root origin to specify the weight of the call."]
                    #[doc = ""]
                    #[doc = "The dispatch origin for this call must be _Root_."]
                    pub fn with_weight(
                        &self,
                        call: super::with_weight::Call,
                        weight: super::with_weight::Weight,
                    ) -> ::subxt::transactions::StaticPayload<super::WithWeight>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Utility",
                            "with_weight",
                            super::WithWeight {
                                call: ::subxt::alloc::boxed::Box::new(call),
                                weight,
                            },
                            [
                                212u8, 91u8, 23u8, 70u8, 94u8, 167u8, 54u8, 234u8, 12u8, 82u8,
                                204u8, 111u8, 211u8, 238u8, 202u8, 190u8, 253u8, 3u8, 180u8, 220u8,
                                179u8, 103u8, 145u8, 46u8, 1u8, 192u8, 130u8, 181u8, 246u8, 44u8,
                                153u8, 9u8,
                            ],
                        )
                    }
                    #[doc = "Dispatch a fallback call in the event the main call fails to execute."]
                    #[doc = "May be called from any origin except `None`."]
                    #[doc = ""]
                    #[doc = "This function first attempts to dispatch the `main` call."]
                    #[doc = "If the `main` call fails, the `fallback` is attemted."]
                    #[doc = "if the fallback is successfully dispatched, the weights of both calls"]
                    #[doc = "are accumulated and an event containing the main call error is deposited."]
                    #[doc = ""]
                    #[doc = "In the event of a fallback failure the whole call fails"]
                    #[doc = "with the weights returned."]
                    #[doc = ""]
                    #[doc = "- `main`: The main call to be dispatched. This is the primary action to execute."]
                    #[doc = "- `fallback`: The fallback call to be dispatched in case the `main` call fails."]
                    #[doc = ""]
                    #[doc = "## Dispatch Logic"]
                    #[doc = "- If the origin is `root`, both the main and fallback calls are executed without"]
                    #[doc = "  applying any origin filters."]
                    #[doc = "- If the origin is not `root`, the origin filter is applied to both the `main` and"]
                    #[doc = "  `fallback` calls."]
                    #[doc = ""]
                    #[doc = "## Use Case"]
                    #[doc = "- Some use cases might involve submitting a `batch` type call in either main, fallback"]
                    #[doc = "  or both."]
                    pub fn if_else(
                        &self,
                        main: super::if_else::Main,
                        fallback: super::if_else::Fallback,
                    ) -> ::subxt::transactions::StaticPayload<super::IfElse> {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Utility",
                            "if_else",
                            super::IfElse {
                                main: ::subxt::alloc::boxed::Box::new(main),
                                fallback: ::subxt::alloc::boxed::Box::new(fallback),
                            },
                            [
                                19u8, 244u8, 45u8, 109u8, 159u8, 48u8, 254u8, 244u8, 168u8, 221u8,
                                167u8, 247u8, 74u8, 191u8, 28u8, 122u8, 224u8, 165u8, 223u8, 251u8,
                                187u8, 53u8, 101u8, 166u8, 13u8, 28u8, 214u8, 217u8, 181u8, 32u8,
                                136u8, 225u8,
                            ],
                        )
                    }
                    #[doc = "Dispatches a function call with a provided origin."]
                    #[doc = ""]
                    #[doc = "Almost the same as [`Pallet::dispatch_as`] but forwards any error of the inner call."]
                    #[doc = ""]
                    #[doc = "The dispatch origin for this call must be _Root_."]
                    pub fn dispatch_as_fallible(
                        &self,
                        as_origin: super::dispatch_as_fallible::AsOrigin,
                        call: super::dispatch_as_fallible::Call,
                    ) -> ::subxt::transactions::StaticPayload<super::DispatchAsFallible>
                    {
                        ::subxt::transactions::StaticPayload::new_static(
                            "Utility",
                            "dispatch_as_fallible",
                            super::DispatchAsFallible {
                                as_origin: ::subxt::alloc::boxed::Box::new(as_origin),
                                call: ::subxt::alloc::boxed::Box::new(call),
                            },
                            [
                                169u8, 252u8, 242u8, 40u8, 56u8, 238u8, 35u8, 231u8, 138u8, 73u8,
                                225u8, 58u8, 127u8, 24u8, 210u8, 143u8, 99u8, 208u8, 104u8, 13u8,
                                54u8, 221u8, 37u8, 129u8, 193u8, 206u8, 196u8, 193u8, 229u8, 144u8,
                                20u8, 129u8,
                            ],
                        )
                    }
                }
            }
        }
        #[doc = "The `Event` enum of this pallet"]
        pub type Event = runtime_types::pallet_utility::pallet::Event;
        pub mod events {
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Batch of dispatches did not complete fully. Index of first failing dispatch given, as"]
            #[doc = "well as the error."]
            pub struct BatchInterrupted {
                pub index: batch_interrupted::Index,
                pub error: batch_interrupted::Error,
            }
            pub mod batch_interrupted {
                use super::runtime_types;
                pub type Index = ::core::primitive::u32;
                pub type Error = runtime_types::sp_runtime::DispatchError;
            }
            impl BatchInterrupted {
                const PALLET_NAME: &'static str = "Utility";
                const EVENT_NAME: &'static str = "BatchInterrupted";
            }
            impl ::subxt::events::DecodeAsEvent for BatchInterrupted {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Batch of dispatches completed fully with no error."]
            pub struct BatchCompleted;
            impl BatchCompleted {
                const PALLET_NAME: &'static str = "Utility";
                const EVENT_NAME: &'static str = "BatchCompleted";
            }
            impl ::subxt::events::DecodeAsEvent for BatchCompleted {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Batch of dispatches completed but has errors."]
            pub struct BatchCompletedWithErrors;
            impl BatchCompletedWithErrors {
                const PALLET_NAME: &'static str = "Utility";
                const EVENT_NAME: &'static str = "BatchCompletedWithErrors";
            }
            impl ::subxt::events::DecodeAsEvent for BatchCompletedWithErrors {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "A single item within a Batch of dispatches has completed with no error."]
            pub struct ItemCompleted;
            impl ItemCompleted {
                const PALLET_NAME: &'static str = "Utility";
                const EVENT_NAME: &'static str = "ItemCompleted";
            }
            impl ::subxt::events::DecodeAsEvent for ItemCompleted {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "A single item within a Batch of dispatches has completed with error."]
            pub struct ItemFailed {
                pub error: item_failed::Error,
            }
            pub mod item_failed {
                use super::runtime_types;
                pub type Error = runtime_types::sp_runtime::DispatchError;
            }
            impl ItemFailed {
                const PALLET_NAME: &'static str = "Utility";
                const EVENT_NAME: &'static str = "ItemFailed";
            }
            impl ::subxt::events::DecodeAsEvent for ItemFailed {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "A call was dispatched."]
            pub struct DispatchedAs {
                pub result: dispatched_as::Result,
            }
            pub mod dispatched_as {
                use super::runtime_types;
                pub type Result =
                    ::core::result::Result<(), runtime_types::sp_runtime::DispatchError>;
            }
            impl DispatchedAs {
                const PALLET_NAME: &'static str = "Utility";
                const EVENT_NAME: &'static str = "DispatchedAs";
            }
            impl ::subxt::events::DecodeAsEvent for DispatchedAs {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "Main call was dispatched."]
            pub struct IfElseMainSuccess;
            impl IfElseMainSuccess {
                const PALLET_NAME: &'static str = "Utility";
                const EVENT_NAME: &'static str = "IfElseMainSuccess";
            }
            impl ::subxt::events::DecodeAsEvent for IfElseMainSuccess {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            #[doc = "The fallback call was dispatched."]
            pub struct IfElseFallbackCalled {
                pub main_error: if_else_fallback_called::MainError,
            }
            pub mod if_else_fallback_called {
                use super::runtime_types;
                pub type MainError = runtime_types::sp_runtime::DispatchError;
            }
            impl IfElseFallbackCalled {
                const PALLET_NAME: &'static str = "Utility";
                const EVENT_NAME: &'static str = "IfElseFallbackCalled";
            }
            impl ::subxt::events::DecodeAsEvent for IfElseFallbackCalled {
                fn is_event(pallet_name: &str, event_name: &str) -> bool {
                    pallet_name == Self::PALLET_NAME && event_name == Self::EVENT_NAME
                }
            }
        }
        pub mod constants {
            use super::runtime_types;
            pub struct ConstantsApi;
            impl ConstantsApi {
                #[doc = " The limit on the number of batched calls."]
                pub fn batched_calls_limit(
                    &self,
                ) -> ::subxt::constants::StaticAddress<::core::primitive::u32> {
                    ::subxt::constants::StaticAddress::new_static(
                        "Utility",
                        "batched_calls_limit",
                        [
                            98u8, 252u8, 116u8, 72u8, 26u8, 180u8, 225u8, 83u8, 200u8, 157u8,
                            125u8, 151u8, 53u8, 76u8, 168u8, 26u8, 10u8, 9u8, 98u8, 68u8, 9u8,
                            178u8, 197u8, 113u8, 31u8, 79u8, 200u8, 90u8, 203u8, 100u8, 41u8,
                            145u8,
                        ],
                    )
                }
            }
        }
    }
    pub mod runtime_types {
        use super::runtime_types;
        pub mod acuity_runtime {
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub enum OriginCaller {
                #[codec(index = 0)]
                system(
                    runtime_types::frame_support::dispatch::RawOrigin<::subxt::utils::AccountId32>,
                ),
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub enum RuntimeCall {
                #[codec(index = 0)]
                System(runtime_types::frame_system::pallet::Call),
                #[codec(index = 1)]
                Timestamp(runtime_types::pallet_timestamp::pallet::Call),
                #[codec(index = 2)]
                ParachainSystem(runtime_types::staging_parachain_info::pallet::Call),
                #[codec(index = 4)]
                Balances(runtime_types::pallet_balances::pallet::Call),
                #[codec(index = 5)]
                Sudo(runtime_types::pallet_sudo::pallet::Call),
                #[codec(index = 7)]
                Content(runtime_types::pallet_content::pallet::Call),
                #[codec(index = 8)]
                AccountContent(runtime_types::pallet_account_content::pallet::Call),
                #[codec(index = 9)]
                AccountProfile(runtime_types::pallet_account_profile::pallet::Call),
                #[codec(index = 10)]
                ContentReactions(runtime_types::pallet_content_reactions::pallet::Call),
                #[codec(index = 11)]
                Utility(runtime_types::pallet_utility::pallet::Call),
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub enum RuntimeError {
                #[codec(index = 0)]
                System(runtime_types::frame_system::pallet::Error),
                #[codec(index = 4)]
                Balances(runtime_types::pallet_balances::pallet::Error),
                #[codec(index = 5)]
                Sudo(runtime_types::pallet_sudo::pallet::Error),
                #[codec(index = 7)]
                Content(runtime_types::pallet_content::pallet::Error),
                #[codec(index = 8)]
                AccountContent(runtime_types::pallet_account_content::pallet::Error),
                #[codec(index = 9)]
                AccountProfile(runtime_types::pallet_account_profile::pallet::Error),
                #[codec(index = 10)]
                ContentReactions(runtime_types::pallet_content_reactions::pallet::Error),
                #[codec(index = 11)]
                Utility(runtime_types::pallet_utility::pallet::Error),
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub enum RuntimeEvent {
                #[codec(index = 0)]
                System(runtime_types::frame_system::pallet::Event),
                #[codec(index = 4)]
                Balances(runtime_types::pallet_balances::pallet::Event),
                #[codec(index = 5)]
                Sudo(runtime_types::pallet_sudo::pallet::Event),
                #[codec(index = 6)]
                TransactionPayment(runtime_types::pallet_transaction_payment::pallet::Event),
                #[codec(index = 7)]
                Content(runtime_types::pallet_content::pallet::Event),
                #[codec(index = 8)]
                AccountContent(runtime_types::pallet_account_content::pallet::Event),
                #[codec(index = 9)]
                AccountProfile(runtime_types::pallet_account_profile::pallet::Event),
                #[codec(index = 10)]
                ContentReactions(runtime_types::pallet_content_reactions::pallet::Event),
                #[codec(index = 11)]
                Utility(runtime_types::pallet_utility::pallet::Event),
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub enum RuntimeHoldReason {}
        }
        pub mod bounded_collections {
            use super::runtime_types;
            pub mod bounded_vec {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct BoundedVec<_0>(pub ::subxt::alloc::vec::Vec<_0>);
            }
            pub mod weak_bounded_vec {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct WeakBoundedVec<_0>(pub ::subxt::alloc::vec::Vec<_0>);
            }
        }
        pub mod frame_support {
            use super::runtime_types;
            pub mod dispatch {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub enum DispatchClass {
                    #[codec(index = 0)]
                    Normal,
                    #[codec(index = 1)]
                    Operational,
                    #[codec(index = 2)]
                    Mandatory,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub enum Pays {
                    #[codec(index = 0)]
                    Yes,
                    #[codec(index = 1)]
                    No,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct PerDispatchClass<_0> {
                    pub normal: _0,
                    pub operational: _0,
                    pub mandatory: _0,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub enum RawOrigin<_0> {
                    #[codec(index = 0)]
                    Root,
                    #[codec(index = 1)]
                    Signed(_0),
                    #[codec(index = 2)]
                    None,
                    #[codec(index = 3)]
                    Authorized,
                }
            }
            pub mod traits {
                use super::runtime_types;
                pub mod storage {
                    use super::runtime_types;
                    #[derive(
                        :: subxt :: ext :: scale_decode :: DecodeAsType,
                        :: subxt :: ext :: scale_encode :: EncodeAsType,
                        Debug,
                    )]
                    #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                    #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                    pub struct NoDrop<_0>(pub _0);
                }
                pub mod tokens {
                    use super::runtime_types;
                    pub mod fungible {
                        use super::runtime_types;
                        pub mod imbalance {
                            use super::runtime_types;
                            #[derive(
                                :: subxt :: ext :: scale_decode :: DecodeAsType,
                                :: subxt :: ext :: scale_encode :: EncodeAsType,
                                Debug,
                            )]
                            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                            pub struct Imbalance<_0> {
                                pub amount: _0,
                            }
                        }
                    }
                    pub mod misc {
                        use super::runtime_types;
                        #[derive(
                            :: subxt :: ext :: scale_decode :: DecodeAsType,
                            :: subxt :: ext :: scale_encode :: EncodeAsType,
                            Debug,
                        )]
                        #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                        #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                        pub enum BalanceStatus {
                            #[codec(index = 0)]
                            Free,
                            #[codec(index = 1)]
                            Reserved,
                        }
                        #[derive(
                            :: subxt :: ext :: scale_decode :: DecodeAsType,
                            :: subxt :: ext :: scale_encode :: EncodeAsType,
                            Debug,
                        )]
                        #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                        #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                        pub struct IdAmount<_0, _1> {
                            pub id: _0,
                            pub amount: _1,
                        }
                    }
                }
            }
        }
        pub mod frame_system {
            use super::runtime_types;
            pub mod extensions {
                use super::runtime_types;
                pub mod authorize_call {
                    use super::runtime_types;
                    #[derive(
                        :: subxt :: ext :: scale_decode :: DecodeAsType,
                        :: subxt :: ext :: scale_encode :: EncodeAsType,
                        Debug,
                    )]
                    #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                    #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                    pub struct AuthorizeCall;
                }
                pub mod check_genesis {
                    use super::runtime_types;
                    #[derive(
                        :: subxt :: ext :: scale_decode :: DecodeAsType,
                        :: subxt :: ext :: scale_encode :: EncodeAsType,
                        Debug,
                    )]
                    #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                    #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                    pub struct CheckGenesis;
                }
                pub mod check_mortality {
                    use super::runtime_types;
                    #[derive(
                        :: subxt :: ext :: scale_decode :: DecodeAsType,
                        :: subxt :: ext :: scale_encode :: EncodeAsType,
                        Debug,
                    )]
                    #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                    #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                    pub struct CheckMortality(pub runtime_types::sp_runtime::generic::era::Era);
                }
                pub mod check_non_zero_sender {
                    use super::runtime_types;
                    #[derive(
                        :: subxt :: ext :: scale_decode :: DecodeAsType,
                        :: subxt :: ext :: scale_encode :: EncodeAsType,
                        Debug,
                    )]
                    #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                    #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                    pub struct CheckNonZeroSender;
                }
                pub mod check_nonce {
                    use super::runtime_types;
                    #[derive(
                        :: subxt :: ext :: scale_decode :: DecodeAsType,
                        :: subxt :: ext :: scale_encode :: EncodeAsType,
                        Debug,
                    )]
                    #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                    #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                    pub struct CheckNonce(#[codec(compact)] pub ::core::primitive::u32);
                }
                pub mod check_spec_version {
                    use super::runtime_types;
                    #[derive(
                        :: subxt :: ext :: scale_decode :: DecodeAsType,
                        :: subxt :: ext :: scale_encode :: EncodeAsType,
                        Debug,
                    )]
                    #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                    #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                    pub struct CheckSpecVersion;
                }
                pub mod check_tx_version {
                    use super::runtime_types;
                    #[derive(
                        :: subxt :: ext :: scale_decode :: DecodeAsType,
                        :: subxt :: ext :: scale_encode :: EncodeAsType,
                        Debug,
                    )]
                    #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                    #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                    pub struct CheckTxVersion;
                }
                pub mod check_weight {
                    use super::runtime_types;
                    #[derive(
                        :: subxt :: ext :: scale_decode :: DecodeAsType,
                        :: subxt :: ext :: scale_encode :: EncodeAsType,
                        Debug,
                    )]
                    #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                    #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                    pub struct CheckWeight;
                }
                pub mod weight_reclaim {
                    use super::runtime_types;
                    #[derive(
                        :: subxt :: ext :: scale_decode :: DecodeAsType,
                        :: subxt :: ext :: scale_encode :: EncodeAsType,
                        Debug,
                    )]
                    #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                    #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                    pub struct WeightReclaim;
                }
            }
            pub mod limits {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct BlockLength {
                    pub max: runtime_types::frame_support::dispatch::PerDispatchClass<
                        ::core::primitive::u32,
                    >,
                    pub max_header_size: ::core::option::Option<::core::primitive::u32>,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct BlockWeights {
                    pub base_block: runtime_types::sp_weights::weight_v2::Weight,
                    pub max_block: runtime_types::sp_weights::weight_v2::Weight,
                    pub per_class: runtime_types::frame_support::dispatch::PerDispatchClass<
                        runtime_types::frame_system::limits::WeightsPerClass,
                    >,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct WeightsPerClass {
                    pub base_extrinsic: runtime_types::sp_weights::weight_v2::Weight,
                    pub max_extrinsic:
                        ::core::option::Option<runtime_types::sp_weights::weight_v2::Weight>,
                    pub max_total:
                        ::core::option::Option<runtime_types::sp_weights::weight_v2::Weight>,
                    pub reserved:
                        ::core::option::Option<runtime_types::sp_weights::weight_v2::Weight>,
                }
            }
            pub mod pallet {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
                pub enum Call {
                    #[codec(index = 0)]
                    #[doc = "Make some on-chain remark."]
                    #[doc = ""]
                    #[doc = "Can be executed by every `origin`."]
                    remark {
                        remark: ::subxt::alloc::vec::Vec<::core::primitive::u8>,
                    },
                    #[codec(index = 1)]
                    #[doc = "Set the number of pages in the WebAssembly environment's heap."]
                    set_heap_pages { pages: ::core::primitive::u64 },
                    #[codec(index = 2)]
                    #[doc = "Set the new runtime code."]
                    set_code {
                        code: ::subxt::alloc::vec::Vec<::core::primitive::u8>,
                    },
                    #[codec(index = 3)]
                    #[doc = "Set the new runtime code without doing any checks of the given `code`."]
                    #[doc = ""]
                    #[doc = "Note that runtime upgrades will not run if this is called with a not-increasing spec"]
                    #[doc = "version!"]
                    set_code_without_checks {
                        code: ::subxt::alloc::vec::Vec<::core::primitive::u8>,
                    },
                    #[codec(index = 4)]
                    #[doc = "Set some items of storage."]
                    set_storage {
                        items: ::subxt::alloc::vec::Vec<(
                            ::subxt::alloc::vec::Vec<::core::primitive::u8>,
                            ::subxt::alloc::vec::Vec<::core::primitive::u8>,
                        )>,
                    },
                    #[codec(index = 5)]
                    #[doc = "Kill some items from storage."]
                    kill_storage {
                        keys: ::subxt::alloc::vec::Vec<
                            ::subxt::alloc::vec::Vec<::core::primitive::u8>,
                        >,
                    },
                    #[codec(index = 6)]
                    #[doc = "Kill all storage items with a key that starts with the given prefix."]
                    #[doc = ""]
                    #[doc = "**NOTE:** We rely on the Root origin to provide us the number of subkeys under"]
                    #[doc = "the prefix we are removing to accurately calculate the weight of this function."]
                    kill_prefix {
                        prefix: ::subxt::alloc::vec::Vec<::core::primitive::u8>,
                        subkeys: ::core::primitive::u32,
                    },
                    #[codec(index = 7)]
                    #[doc = "Make some on-chain remark and emit event."]
                    remark_with_event {
                        remark: ::subxt::alloc::vec::Vec<::core::primitive::u8>,
                    },
                    #[codec(index = 9)]
                    #[doc = "Authorize an upgrade to a given `code_hash` for the runtime. The runtime can be supplied"]
                    #[doc = "later."]
                    #[doc = ""]
                    #[doc = "This call requires Root origin."]
                    authorize_upgrade { code_hash: ::subxt::utils::H256 },
                    #[codec(index = 10)]
                    #[doc = "Authorize an upgrade to a given `code_hash` for the runtime. The runtime can be supplied"]
                    #[doc = "later."]
                    #[doc = ""]
                    #[doc = "WARNING: This authorizes an upgrade that will take place without any safety checks, for"]
                    #[doc = "example that the spec name remains the same and that the version number increases. Not"]
                    #[doc = "recommended for normal use. Use `authorize_upgrade` instead."]
                    #[doc = ""]
                    #[doc = "This call requires Root origin."]
                    authorize_upgrade_without_checks { code_hash: ::subxt::utils::H256 },
                    #[codec(index = 11)]
                    #[doc = "Provide the preimage (runtime binary) `code` for an upgrade that has been authorized."]
                    #[doc = ""]
                    #[doc = "If the authorization required a version check, this call will ensure the spec name"]
                    #[doc = "remains unchanged and that the spec version has increased."]
                    #[doc = ""]
                    #[doc = "Depending on the runtime's `OnSetCode` configuration, this function may directly apply"]
                    #[doc = "the new `code` in the same block or attempt to schedule the upgrade."]
                    #[doc = ""]
                    #[doc = "All origins are allowed."]
                    apply_authorized_upgrade {
                        code: ::subxt::alloc::vec::Vec<::core::primitive::u8>,
                    },
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "Error for the System pallet"]
                pub enum Error {
                    #[codec(index = 0)]
                    #[doc = "The name of specification does not match between the current runtime"]
                    #[doc = "and the new runtime."]
                    InvalidSpecName,
                    #[codec(index = 1)]
                    #[doc = "The specification version is not allowed to decrease between the current runtime"]
                    #[doc = "and the new runtime."]
                    SpecVersionNeedsToIncrease,
                    #[codec(index = 2)]
                    #[doc = "Failed to extract the runtime version from the new runtime."]
                    #[doc = ""]
                    #[doc = "Either calling `Core_version` or decoding `RuntimeVersion` failed."]
                    FailedToExtractRuntimeVersion,
                    #[codec(index = 3)]
                    #[doc = "Suicide called when the account has non-default composite data."]
                    NonDefaultComposite,
                    #[codec(index = 4)]
                    #[doc = "There is a non-zero reference count preventing the account from being purged."]
                    NonZeroRefCount,
                    #[codec(index = 5)]
                    #[doc = "The origin filter prevent the call to be dispatched."]
                    CallFiltered,
                    #[codec(index = 6)]
                    #[doc = "A multi-block migration is ongoing and prevents the current code from being replaced."]
                    MultiBlockMigrationsOngoing,
                    #[codec(index = 7)]
                    #[doc = "No upgrade authorized."]
                    NothingAuthorized,
                    #[codec(index = 8)]
                    #[doc = "The submitted code is not authorized."]
                    Unauthorized,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "Event for the System pallet."]
                pub enum Event {
                    #[codec(index = 0)]
                    #[doc = "An extrinsic completed successfully."]
                    ExtrinsicSuccess {
                        dispatch_info: runtime_types::frame_system::DispatchEventInfo,
                    },
                    #[codec(index = 1)]
                    #[doc = "An extrinsic failed."]
                    ExtrinsicFailed {
                        dispatch_error: runtime_types::sp_runtime::DispatchError,
                        dispatch_info: runtime_types::frame_system::DispatchEventInfo,
                    },
                    #[codec(index = 2)]
                    #[doc = "`:code` was updated to the code with the given hash."]
                    CodeUpdated { hash: ::subxt::utils::H256 },
                    #[codec(index = 3)]
                    #[doc = "A new account was created."]
                    NewAccount {
                        account: ::subxt::utils::AccountId32,
                    },
                    #[codec(index = 4)]
                    #[doc = "An account was reaped."]
                    KilledAccount {
                        account: ::subxt::utils::AccountId32,
                    },
                    #[codec(index = 5)]
                    #[doc = "On on-chain remark happened."]
                    Remarked {
                        sender: ::subxt::utils::AccountId32,
                        hash: ::subxt::utils::H256,
                    },
                    #[codec(index = 6)]
                    #[doc = "An upgrade was authorized."]
                    UpgradeAuthorized {
                        code_hash: ::subxt::utils::H256,
                        check_version: ::core::primitive::bool,
                    },
                    #[codec(index = 7)]
                    #[doc = "An invalid authorized upgrade was rejected while trying to apply it."]
                    RejectedInvalidAuthorizedUpgrade {
                        code_hash: ::subxt::utils::H256,
                        error: runtime_types::sp_runtime::DispatchError,
                    },
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub struct AccountInfo<_0, _1> {
                pub nonce: _0,
                pub consumers: ::core::primitive::u32,
                pub providers: ::core::primitive::u32,
                pub sufficients: ::core::primitive::u32,
                pub data: _1,
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub struct CodeUpgradeAuthorization {
                pub code_hash: ::subxt::utils::H256,
                pub check_version: ::core::primitive::bool,
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub struct DispatchEventInfo {
                pub weight: runtime_types::sp_weights::weight_v2::Weight,
                pub class: runtime_types::frame_support::dispatch::DispatchClass,
                pub pays_fee: runtime_types::frame_support::dispatch::Pays,
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub struct EventRecord<_0, _1> {
                pub phase: runtime_types::frame_system::Phase,
                pub event: _0,
                pub topics: ::subxt::alloc::vec::Vec<_1>,
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub struct LastRuntimeUpgradeInfo {
                #[codec(compact)]
                pub spec_version: ::core::primitive::u32,
                pub spec_name: ::subxt::alloc::string::String,
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub enum Phase {
                #[codec(index = 0)]
                ApplyExtrinsic(::core::primitive::u32),
                #[codec(index = 1)]
                Finalization,
                #[codec(index = 2)]
                Initialization,
            }
        }
        pub mod pallet_account_content {
            use super::runtime_types;
            pub mod pallet {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
                pub enum Call {
                    #[codec(index = 0)]
                    #[doc = "Adds a content item to the caller's ordered list."]
                    #[doc = ""]
                    #[doc = "The referenced item must exist in `pallet-content`, must not be"]
                    #[doc = "retracted, and must currently be owned by the caller."]
                    add_item {
                        item_id: runtime_types::pallet_content::pallet::ItemId,
                    },
                    #[codec(index = 1)]
                    #[doc = "Removes a content item from the caller's ordered list."]
                    #[doc = ""]
                    #[doc = "Removal uses swap-with-last semantics so membership checks and"]
                    #[doc = "deletions stay O(1)."]
                    remove_item {
                        item_id: runtime_types::pallet_content::pallet::ItemId,
                    },
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "Errors returned by the account-content pallet."]
                pub enum Error {
                    #[codec(index = 0)]
                    #[doc = "The item is already in the account list."]
                    ItemAlreadyAdded,
                    #[codec(index = 1)]
                    #[doc = "The item is not in the account list."]
                    ItemNotAdded,
                    #[codec(index = 2)]
                    #[doc = "The referenced content item could not be found."]
                    ItemNotFound,
                    #[codec(index = 3)]
                    #[doc = "The referenced content item has been retracted."]
                    ItemRetracted,
                    #[codec(index = 4)]
                    #[doc = "The signer does not own the referenced content item."]
                    WrongAccount,
                    #[codec(index = 5)]
                    #[doc = "The account has reached the maximum supported number of items."]
                    AccountItemsFull,
                    #[codec(index = 6)]
                    #[doc = "A stored index could not be converted on this platform."]
                    IndexOverflow,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "The `Event` enum of this pallet"]
                pub enum Event {
                    #[codec(index = 0)]
                    #[doc = "An item was added to an account list."]
                    AddItem {
                        account: ::subxt::utils::AccountId32,
                        item_id: runtime_types::pallet_content::pallet::ItemId,
                    },
                    #[codec(index = 1)]
                    #[doc = "An item was removed from an account list."]
                    RemoveItem {
                        account: ::subxt::utils::AccountId32,
                        item_id: runtime_types::pallet_content::pallet::ItemId,
                    },
                }
            }
        }
        pub mod pallet_account_profile {
            use super::runtime_types;
            pub mod pallet {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
                pub enum Call {
                    #[codec(index = 0)]
                    #[doc = "Sets or overwrites the caller's profile pointer."]
                    #[doc = ""]
                    #[doc = "The referenced item must exist in `pallet-content`, must not be"]
                    #[doc = "retracted, and must currently be owned by the caller."]
                    set_profile {
                        item_id: runtime_types::pallet_content::pallet::ItemId,
                    },
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "Errors returned by the account-profile pallet."]
                pub enum Error {
                    #[codec(index = 0)]
                    #[doc = "The referenced content item could not be found."]
                    ItemNotFound,
                    #[codec(index = 1)]
                    #[doc = "The referenced content item has been retracted."]
                    ItemRetracted,
                    #[codec(index = 2)]
                    #[doc = "The signer does not own the referenced content item."]
                    WrongAccount,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "The `Event` enum of this pallet"]
                pub enum Event {
                    #[codec(index = 0)]
                    #[doc = "A profile pointer was set or replaced."]
                    ProfileSet {
                        account: ::subxt::utils::AccountId32,
                        item_id: runtime_types::pallet_content::pallet::ItemId,
                    },
                }
            }
        }
        pub mod pallet_balances {
            use super::runtime_types;
            pub mod pallet {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
                pub enum Call {
                    #[codec(index = 0)]
                    #[doc = "Transfer some liquid free balance to another account."]
                    #[doc = ""]
                    #[doc = "`transfer_allow_death` will set the `FreeBalance` of the sender and receiver."]
                    #[doc = "If the sender's account is below the existential deposit as a result"]
                    #[doc = "of the transfer, the account will be reaped."]
                    #[doc = ""]
                    #[doc = "The dispatch origin for this call must be `Signed` by the transactor."]
                    transfer_allow_death {
                        dest: ::subxt::utils::MultiAddress<::subxt::utils::AccountId32, ()>,
                        #[codec(compact)]
                        value: ::core::primitive::u128,
                    },
                    #[codec(index = 2)]
                    #[doc = "Exactly as `transfer_allow_death`, except the origin must be root and the source account"]
                    #[doc = "may be specified."]
                    force_transfer {
                        source: ::subxt::utils::MultiAddress<::subxt::utils::AccountId32, ()>,
                        dest: ::subxt::utils::MultiAddress<::subxt::utils::AccountId32, ()>,
                        #[codec(compact)]
                        value: ::core::primitive::u128,
                    },
                    #[codec(index = 3)]
                    #[doc = "Same as the [`transfer_allow_death`] call, but with a check that the transfer will not"]
                    #[doc = "kill the origin account."]
                    #[doc = ""]
                    #[doc = "99% of the time you want [`transfer_allow_death`] instead."]
                    #[doc = ""]
                    #[doc = "[`transfer_allow_death`]: struct.Pallet.html#method.transfer"]
                    transfer_keep_alive {
                        dest: ::subxt::utils::MultiAddress<::subxt::utils::AccountId32, ()>,
                        #[codec(compact)]
                        value: ::core::primitive::u128,
                    },
                    #[codec(index = 4)]
                    #[doc = "Transfer the entire transferable balance from the caller account."]
                    #[doc = ""]
                    #[doc = "NOTE: This function only attempts to transfer _transferable_ balances. This means that"]
                    #[doc = "any locked, reserved, or existential deposits (when `keep_alive` is `true`), will not be"]
                    #[doc = "transferred by this function. To ensure that this function results in a killed account,"]
                    #[doc = "you might need to prepare the account by removing any reference counters, storage"]
                    #[doc = "deposits, etc..."]
                    #[doc = ""]
                    #[doc = "The dispatch origin of this call must be Signed."]
                    #[doc = ""]
                    #[doc = "- `dest`: The recipient of the transfer."]
                    #[doc = "- `keep_alive`: A boolean to determine if the `transfer_all` operation should send all"]
                    #[doc = "  of the funds the account has, causing the sender account to be killed (false), or"]
                    #[doc = "  transfer everything except at least the existential deposit, which will guarantee to"]
                    #[doc = "  keep the sender account alive (true)."]
                    transfer_all {
                        dest: ::subxt::utils::MultiAddress<::subxt::utils::AccountId32, ()>,
                        keep_alive: ::core::primitive::bool,
                    },
                    #[codec(index = 5)]
                    #[doc = "Unreserve some balance from a user by force."]
                    #[doc = ""]
                    #[doc = "Can only be called by ROOT."]
                    force_unreserve {
                        who: ::subxt::utils::MultiAddress<::subxt::utils::AccountId32, ()>,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 6)]
                    #[doc = "Upgrade a specified account."]
                    #[doc = ""]
                    #[doc = "- `origin`: Must be `Signed`."]
                    #[doc = "- `who`: The account to be upgraded."]
                    #[doc = ""]
                    #[doc = "This will waive the transaction fee if at least all but 10% of the accounts needed to"]
                    #[doc = "be upgraded. (We let some not have to be upgraded just in order to allow for the"]
                    #[doc = "possibility of churn)."]
                    upgrade_accounts {
                        who: ::subxt::alloc::vec::Vec<::subxt::utils::AccountId32>,
                    },
                    #[codec(index = 8)]
                    #[doc = "Set the regular balance of a given account."]
                    #[doc = ""]
                    #[doc = "The dispatch origin for this call is `root`."]
                    force_set_balance {
                        who: ::subxt::utils::MultiAddress<::subxt::utils::AccountId32, ()>,
                        #[codec(compact)]
                        new_free: ::core::primitive::u128,
                    },
                    #[codec(index = 9)]
                    #[doc = "Adjust the total issuance in a saturating way."]
                    #[doc = ""]
                    #[doc = "Can only be called by root and always needs a positive `delta`."]
                    #[doc = ""]
                    #[doc = "# Example"]
                    force_adjust_total_issuance {
                        direction: runtime_types::pallet_balances::types::AdjustmentDirection,
                        #[codec(compact)]
                        delta: ::core::primitive::u128,
                    },
                    #[codec(index = 10)]
                    #[doc = "Burn the specified liquid free balance from the origin account."]
                    #[doc = ""]
                    #[doc = "If the origin's account ends up below the existential deposit as a result"]
                    #[doc = "of the burn and `keep_alive` is false, the account will be reaped."]
                    #[doc = ""]
                    #[doc = "Unlike sending funds to a _burn_ address, which merely makes the funds inaccessible,"]
                    #[doc = "this `burn` operation will reduce total issuance by the amount _burned_."]
                    burn {
                        #[codec(compact)]
                        value: ::core::primitive::u128,
                        keep_alive: ::core::primitive::bool,
                    },
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "The `Error` enum of this pallet."]
                pub enum Error {
                    #[codec(index = 0)]
                    #[doc = "Vesting balance too high to send value."]
                    VestingBalance,
                    #[codec(index = 1)]
                    #[doc = "Account liquidity restrictions prevent withdrawal."]
                    LiquidityRestrictions,
                    #[codec(index = 2)]
                    #[doc = "Balance too low to send value."]
                    InsufficientBalance,
                    #[codec(index = 3)]
                    #[doc = "Value too low to create account due to existential deposit."]
                    ExistentialDeposit,
                    #[codec(index = 4)]
                    #[doc = "Transfer/payment would kill account."]
                    Expendability,
                    #[codec(index = 5)]
                    #[doc = "A vesting schedule already exists for this account."]
                    ExistingVestingSchedule,
                    #[codec(index = 6)]
                    #[doc = "Beneficiary account must pre-exist."]
                    DeadAccount,
                    #[codec(index = 7)]
                    #[doc = "Number of named reserves exceed `MaxReserves`."]
                    TooManyReserves,
                    #[codec(index = 8)]
                    #[doc = "Number of holds exceed `VariantCountOf<T::RuntimeHoldReason>`."]
                    TooManyHolds,
                    #[codec(index = 9)]
                    #[doc = "Number of freezes exceed `MaxFreezes`."]
                    TooManyFreezes,
                    #[codec(index = 10)]
                    #[doc = "The issuance cannot be modified since it is already deactivated."]
                    IssuanceDeactivated,
                    #[codec(index = 11)]
                    #[doc = "The delta cannot be zero."]
                    DeltaZero,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "The `Event` enum of this pallet"]
                pub enum Event {
                    #[codec(index = 0)]
                    #[doc = "An account was created with some free balance."]
                    Endowed {
                        account: ::subxt::utils::AccountId32,
                        free_balance: ::core::primitive::u128,
                    },
                    #[codec(index = 1)]
                    #[doc = "An account was removed whose balance was non-zero but below ExistentialDeposit,"]
                    #[doc = "resulting in an outright loss."]
                    DustLost {
                        account: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 2)]
                    #[doc = "Transfer succeeded."]
                    Transfer {
                        from: ::subxt::utils::AccountId32,
                        to: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 3)]
                    #[doc = "A balance was set by root."]
                    BalanceSet {
                        who: ::subxt::utils::AccountId32,
                        free: ::core::primitive::u128,
                    },
                    #[codec(index = 4)]
                    #[doc = "Some balance was reserved (moved from free to reserved)."]
                    Reserved {
                        who: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 5)]
                    #[doc = "Some balance was unreserved (moved from reserved to free)."]
                    Unreserved {
                        who: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 6)]
                    #[doc = "Some balance was moved from the reserve of the first account to the second account."]
                    #[doc = "Final argument indicates the destination balance type."]
                    ReserveRepatriated {
                        from: ::subxt::utils::AccountId32,
                        to: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                        destination_status:
                            runtime_types::frame_support::traits::tokens::misc::BalanceStatus,
                    },
                    #[codec(index = 7)]
                    #[doc = "Some amount was deposited (e.g. for transaction fees)."]
                    Deposit {
                        who: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 8)]
                    #[doc = "Some amount was withdrawn from the account (e.g. for transaction fees)."]
                    Withdraw {
                        who: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 9)]
                    #[doc = "Some amount was removed from the account (e.g. for misbehavior)."]
                    Slashed {
                        who: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 10)]
                    #[doc = "Some amount was minted into an account."]
                    Minted {
                        who: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 11)]
                    #[doc = "Some credit was balanced and added to the TotalIssuance."]
                    MintedCredit { amount: ::core::primitive::u128 },
                    #[codec(index = 12)]
                    #[doc = "Some amount was burned from an account."]
                    Burned {
                        who: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 13)]
                    #[doc = "Some debt has been dropped from the Total Issuance."]
                    BurnedDebt { amount: ::core::primitive::u128 },
                    #[codec(index = 14)]
                    #[doc = "Some amount was suspended from an account (it can be restored later)."]
                    Suspended {
                        who: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 15)]
                    #[doc = "Some amount was restored into an account."]
                    Restored {
                        who: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 16)]
                    #[doc = "An account was upgraded."]
                    Upgraded { who: ::subxt::utils::AccountId32 },
                    #[codec(index = 17)]
                    #[doc = "Total issuance was increased by `amount`, creating a credit to be balanced."]
                    Issued { amount: ::core::primitive::u128 },
                    #[codec(index = 18)]
                    #[doc = "Total issuance was decreased by `amount`, creating a debt to be balanced."]
                    Rescinded { amount: ::core::primitive::u128 },
                    #[codec(index = 19)]
                    #[doc = "Some balance was locked."]
                    Locked {
                        who: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 20)]
                    #[doc = "Some balance was unlocked."]
                    Unlocked {
                        who: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 21)]
                    #[doc = "Some balance was frozen."]
                    Frozen {
                        who: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 22)]
                    #[doc = "Some balance was thawed."]
                    Thawed {
                        who: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 23)]
                    #[doc = "The `TotalIssuance` was forcefully changed."]
                    TotalIssuanceForced {
                        old: ::core::primitive::u128,
                        new: ::core::primitive::u128,
                    },
                    #[codec(index = 24)]
                    #[doc = "Some balance was placed on hold."]
                    Held {
                        reason: runtime_types::acuity_runtime::RuntimeHoldReason,
                        who: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 25)]
                    #[doc = "Held balance was burned from an account."]
                    BurnedHeld {
                        reason: runtime_types::acuity_runtime::RuntimeHoldReason,
                        who: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 26)]
                    #[doc = "A transfer of `amount` on hold from `source` to `dest` was initiated."]
                    TransferOnHold {
                        reason: runtime_types::acuity_runtime::RuntimeHoldReason,
                        source: ::subxt::utils::AccountId32,
                        dest: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 27)]
                    #[doc = "The `transferred` balance is placed on hold at the `dest` account."]
                    TransferAndHold {
                        reason: runtime_types::acuity_runtime::RuntimeHoldReason,
                        source: ::subxt::utils::AccountId32,
                        dest: ::subxt::utils::AccountId32,
                        transferred: ::core::primitive::u128,
                    },
                    #[codec(index = 28)]
                    #[doc = "Some balance was released from hold."]
                    Released {
                        reason: runtime_types::acuity_runtime::RuntimeHoldReason,
                        who: ::subxt::utils::AccountId32,
                        amount: ::core::primitive::u128,
                    },
                    #[codec(index = 29)]
                    #[doc = "An unexpected/defensive event was triggered."]
                    Unexpected(runtime_types::pallet_balances::pallet::UnexpectedKind),
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub enum UnexpectedKind {
                    #[codec(index = 0)]
                    BalanceUpdated,
                    #[codec(index = 1)]
                    FailedToMutateAccount,
                }
            }
            pub mod types {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct AccountData<_0> {
                    pub free: _0,
                    pub reserved: _0,
                    pub frozen: _0,
                    pub flags: runtime_types::pallet_balances::types::ExtraFlags,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub enum AdjustmentDirection {
                    #[codec(index = 0)]
                    Increase,
                    #[codec(index = 1)]
                    Decrease,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct BalanceLock<_0> {
                    pub id: [::core::primitive::u8; 8usize],
                    pub amount: _0,
                    pub reasons: runtime_types::pallet_balances::types::Reasons,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct ExtraFlags(pub ::core::primitive::u128);
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub enum Reasons {
                    #[codec(index = 0)]
                    Fee,
                    #[codec(index = 1)]
                    Misc,
                    #[codec(index = 2)]
                    All,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct ReserveData<_0, _1> {
                    pub id: _0,
                    pub amount: _1,
                }
            }
        }
        pub mod pallet_content {
            use super::runtime_types;
            pub mod pallet {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
                pub enum Call {
                    #[codec(index = 0)]
                    #[doc = "Publishes a new item and its initial revision."]
                    #[doc = ""]
                    #[doc = "The item id is derived from the signer, the supplied [`Nonce`], and"]
                    #[doc = "[`Config::ItemIdNamespace`]. The call persists only ownership,"]
                    #[doc = "revision, and flag metadata; graph edges and the payload reference are"]
                    #[doc = "emitted in events for off-chain indexing."]
                    publish_item {
                        nonce: runtime_types::pallet_content::Nonce,
                        parents: runtime_types::bounded_collections::bounded_vec::BoundedVec<
                            runtime_types::pallet_content::pallet::ItemId,
                        >,
                        flags: ::core::primitive::u8,
                        links: runtime_types::bounded_collections::bounded_vec::BoundedVec<
                            runtime_types::pallet_content::pallet::ItemId,
                        >,
                        mentions: runtime_types::bounded_collections::bounded_vec::BoundedVec<
                            ::subxt::utils::AccountId32,
                        >,
                        ipfs_hash: runtime_types::pallet_content::pallet::IpfsHash,
                    },
                    #[codec(index = 1)]
                    #[doc = "Publishes a new revision for an existing item."]
                    #[doc = ""]
                    #[doc = "Only the current item owner can publish revisions, and only while the"]
                    #[doc = "item is marked [`REVISIONABLE`] and not [`RETRACTED`]."]
                    publish_revision {
                        item_id: runtime_types::pallet_content::pallet::ItemId,
                        links: runtime_types::bounded_collections::bounded_vec::BoundedVec<
                            runtime_types::pallet_content::pallet::ItemId,
                        >,
                        mentions: runtime_types::bounded_collections::bounded_vec::BoundedVec<
                            ::subxt::utils::AccountId32,
                        >,
                        ipfs_hash: runtime_types::pallet_content::pallet::IpfsHash,
                    },
                    #[codec(index = 2)]
                    #[doc = "Marks an item as retracted."]
                    #[doc = ""]
                    #[doc = "Only the owner can retract, and only while the item still has the"]
                    #[doc = "[`RETRACTABLE`] permission bit set."]
                    retract_item {
                        item_id: runtime_types::pallet_content::pallet::ItemId,
                    },
                    #[codec(index = 3)]
                    #[doc = "Permanently disables future revisions for an item."]
                    set_not_revisionable {
                        item_id: runtime_types::pallet_content::pallet::ItemId,
                    },
                    #[codec(index = 4)]
                    #[doc = "Permanently disables future retraction for an item."]
                    set_not_retractable {
                        item_id: runtime_types::pallet_content::pallet::ItemId,
                    },
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "Errors returned by the content pallet."]
                pub enum Error {
                    #[codec(index = 0)]
                    #[doc = "The item already exists."]
                    ItemAlreadyExists,
                    #[codec(index = 1)]
                    #[doc = "The item could not be found."]
                    ItemNotFound,
                    #[codec(index = 2)]
                    #[doc = "The item has been retracted."]
                    ItemRetracted,
                    #[codec(index = 3)]
                    #[doc = "The item is not revisionable."]
                    ItemNotRevisionable,
                    #[codec(index = 4)]
                    #[doc = "The item is not retractable."]
                    ItemNotRetractable,
                    #[codec(index = 5)]
                    #[doc = "Wrong account."]
                    WrongAccount,
                    #[codec(index = 6)]
                    #[doc = "Flags contain unsupported bits."]
                    InvalidFlags,
                    #[codec(index = 7)]
                    #[doc = "Revision id overflowed."]
                    RevisionIdOverflow,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "The `Event` enum of this pallet"]
                pub enum Event {
                    #[codec(index = 0)]
                    #[doc = "A new item was created."]
                    PublishItem {
                        item_id: runtime_types::pallet_content::pallet::ItemId,
                        owner: ::subxt::utils::AccountId32,
                        parents: runtime_types::bounded_collections::bounded_vec::BoundedVec<
                            runtime_types::pallet_content::pallet::ItemId,
                        >,
                        flags: ::core::primitive::u8,
                    },
                    #[codec(index = 1)]
                    #[doc = "A new revision was published for an item."]
                    PublishRevision {
                        item_id: runtime_types::pallet_content::pallet::ItemId,
                        owner: ::subxt::utils::AccountId32,
                        revision_id: ::core::primitive::u32,
                        links: runtime_types::bounded_collections::bounded_vec::BoundedVec<
                            runtime_types::pallet_content::pallet::ItemId,
                        >,
                        mentions: runtime_types::bounded_collections::bounded_vec::BoundedVec<
                            ::subxt::utils::AccountId32,
                        >,
                        ipfs_hash: runtime_types::pallet_content::pallet::IpfsHash,
                    },
                    #[codec(index = 2)]
                    #[doc = "An item was marked as retracted."]
                    RetractItem {
                        item_id: runtime_types::pallet_content::pallet::ItemId,
                        owner: ::subxt::utils::AccountId32,
                    },
                    #[codec(index = 3)]
                    #[doc = "Revision publishing was permanently disabled for an item."]
                    SetNotRevsionable {
                        item_id: runtime_types::pallet_content::pallet::ItemId,
                        owner: ::subxt::utils::AccountId32,
                    },
                    #[codec(index = 4)]
                    #[doc = "Retraction was permanently disabled for an item."]
                    SetNotRetractable {
                        item_id: runtime_types::pallet_content::pallet::ItemId,
                        owner: ::subxt::utils::AccountId32,
                    },
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct IpfsHash(pub [::core::primitive::u8; 32usize]);
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct Item<_0> {
                    pub owner: _0,
                    pub revision_id: ::core::primitive::u32,
                    pub flags: ::core::primitive::u8,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct ItemId(pub [::core::primitive::u8; 32usize]);
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub struct Nonce(pub [::core::primitive::u8; 32usize]);
        }
        pub mod pallet_content_reactions {
            use super::runtime_types;
            pub mod pallet {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
                pub enum Call {
                    #[codec(index = 0)]
                    #[doc = "Sets the caller's full reaction set for a specific item revision,"]
                    #[doc = "replacing any prior reactions."]
                    #[doc = ""]
                    #[doc = "The entire reaction set is emitted in a single `SetReactions` event."]
                    #[doc = "Duplicates within the provided set are rejected."]
                    set_reactions {
                        item_id: runtime_types::pallet_content::pallet::ItemId,
                        revision_id: ::core::primitive::u32,
                        reactions: runtime_types::bounded_collections::bounded_vec::BoundedVec<
                            runtime_types::pallet_content_reactions::pallet::Emoji,
                        >,
                    },
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct Emoji(pub ::core::primitive::u32);
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "Errors returned by the content-reactions pallet."]
                pub enum Error {
                    #[codec(index = 0)]
                    #[doc = "The referenced content item could not be found."]
                    ItemNotFound,
                    #[codec(index = 1)]
                    #[doc = "The referenced content item has been retracted."]
                    ItemRetracted,
                    #[codec(index = 2)]
                    #[doc = "The referenced revision could not be found for the item."]
                    RevisionNotFound,
                    #[codec(index = 3)]
                    #[doc = "The provided emoji value is not a valid non-zero Unicode scalar value."]
                    InvalidEmoji,
                    #[codec(index = 4)]
                    #[doc = "The provided reaction set contains duplicate emojis."]
                    DuplicateEmoji,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "The `Event` enum of this pallet"]
                pub enum Event {
                    #[codec(index = 0)]
                    #[doc = "The caller's reaction set for a specific item revision was set."]
                    SetReactions {
                        item_id: runtime_types::pallet_content::pallet::ItemId,
                        revision_id: ::core::primitive::u32,
                        item_owner: ::subxt::utils::AccountId32,
                        reactor: ::subxt::utils::AccountId32,
                        reactions: runtime_types::bounded_collections::bounded_vec::BoundedVec<
                            runtime_types::pallet_content_reactions::pallet::Emoji,
                        >,
                    },
                }
            }
        }
        pub mod pallet_sudo {
            use super::runtime_types;
            pub mod pallet {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
                pub enum Call {
                    #[codec(index = 0)]
                    #[doc = "Authenticates the sudo key and dispatches a function call with `Root` origin."]
                    sudo {
                        call:
                            ::subxt::alloc::boxed::Box<runtime_types::acuity_runtime::RuntimeCall>,
                    },
                    #[codec(index = 1)]
                    #[doc = "Authenticates the sudo key and dispatches a function call with `Root` origin."]
                    #[doc = "This function does not check the weight of the call, and instead allows the"]
                    #[doc = "Sudo user to specify the weight of the call."]
                    #[doc = ""]
                    #[doc = "The dispatch origin for this call must be _Signed_."]
                    sudo_unchecked_weight {
                        call:
                            ::subxt::alloc::boxed::Box<runtime_types::acuity_runtime::RuntimeCall>,
                        weight: runtime_types::sp_weights::weight_v2::Weight,
                    },
                    #[codec(index = 2)]
                    #[doc = "Authenticates the current sudo key and sets the given AccountId (`new`) as the new sudo"]
                    #[doc = "key."]
                    set_key {
                        new: ::subxt::utils::MultiAddress<::subxt::utils::AccountId32, ()>,
                    },
                    #[codec(index = 3)]
                    #[doc = "Authenticates the sudo key and dispatches a function call with `Signed` origin from"]
                    #[doc = "a given account."]
                    #[doc = ""]
                    #[doc = "The dispatch origin for this call must be _Signed_."]
                    sudo_as {
                        who: ::subxt::utils::MultiAddress<::subxt::utils::AccountId32, ()>,
                        call:
                            ::subxt::alloc::boxed::Box<runtime_types::acuity_runtime::RuntimeCall>,
                    },
                    #[codec(index = 4)]
                    #[doc = "Permanently removes the sudo key."]
                    #[doc = ""]
                    #[doc = "**This cannot be un-done.**"]
                    remove_key,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "Error for the Sudo pallet."]
                pub enum Error {
                    #[codec(index = 0)]
                    #[doc = "Sender must be the Sudo account."]
                    RequireSudo,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "The `Event` enum of this pallet"]
                pub enum Event {
                    #[codec(index = 0)]
                    #[doc = "A sudo call just took place."]
                    Sudid {
                        sudo_result:
                            ::core::result::Result<(), runtime_types::sp_runtime::DispatchError>,
                    },
                    #[codec(index = 1)]
                    #[doc = "The sudo key has been updated."]
                    KeyChanged {
                        old: ::core::option::Option<::subxt::utils::AccountId32>,
                        new: ::subxt::utils::AccountId32,
                    },
                    #[codec(index = 2)]
                    #[doc = "The key was permanently removed."]
                    KeyRemoved,
                    #[codec(index = 3)]
                    #[doc = "A [sudo_as](Pallet::sudo_as) call just took place."]
                    SudoAsDone {
                        sudo_result:
                            ::core::result::Result<(), runtime_types::sp_runtime::DispatchError>,
                    },
                }
            }
        }
        pub mod pallet_timestamp {
            use super::runtime_types;
            pub mod pallet {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
                pub enum Call {
                    #[codec(index = 0)]
                    #[doc = "Set the current time."]
                    #[doc = ""]
                    #[doc = "This call should be invoked exactly once per block. It will panic at the finalization"]
                    #[doc = "phase, if this call hasn't been invoked by that time."]
                    #[doc = ""]
                    #[doc = "The timestamp should be greater than the previous one by the amount specified by"]
                    #[doc = "[`Config::MinimumPeriod`]."]
                    #[doc = ""]
                    #[doc = "The dispatch origin for this call must be _None_."]
                    #[doc = ""]
                    #[doc = "This dispatch class is _Mandatory_ to ensure it gets executed in the block. Be aware"]
                    #[doc = "that changing the complexity of this call could result exhausting the resources in a"]
                    #[doc = "block to execute any other calls."]
                    #[doc = ""]
                    #[doc = "## Complexity"]
                    #[doc = "- `O(1)` (Note that implementations of `OnTimestampSet` must also be `O(1)`)"]
                    #[doc = "- 1 storage read and 1 storage mutation (codec `O(1)` because of `DidUpdate::take` in"]
                    #[doc = "  `on_finalize`)"]
                    #[doc = "- 1 event handler `on_timestamp_set`. Must be `O(1)`."]
                    set {
                        #[codec(compact)]
                        now: ::core::primitive::u64,
                    },
                }
            }
        }
        pub mod pallet_transaction_payment {
            use super::runtime_types;
            pub mod pallet {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "The `Event` enum of this pallet"]
                pub enum Event {
                    #[codec(index = 0)]
                    #[doc = "A transaction fee `actual_fee`, of which `tip` was added to the minimum inclusion fee,"]
                    #[doc = "has been paid by `who`."]
                    TransactionFeePaid {
                        who: ::subxt::utils::AccountId32,
                        actual_fee: ::core::primitive::u128,
                        tip: ::core::primitive::u128,
                    },
                }
            }
            pub mod types {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct FeeDetails<_0> {
                    pub inclusion_fee: ::core::option::Option<
                        runtime_types::pallet_transaction_payment::types::InclusionFee<_0>,
                    >,
                    pub tip: _0,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct InclusionFee<_0> {
                    pub base_fee: _0,
                    pub len_fee: _0,
                    pub adjusted_weight_fee: _0,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct RuntimeDispatchInfo<_0, _1> {
                    pub weight: _1,
                    pub class: runtime_types::frame_support::dispatch::DispatchClass,
                    pub partial_fee: _0,
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub struct ChargeTransactionPayment(#[codec(compact)] pub ::core::primitive::u128);
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub enum Releases {
                #[codec(index = 0)]
                V1Ancient,
                #[codec(index = 1)]
                V2,
            }
        }
        pub mod pallet_utility {
            use super::runtime_types;
            pub mod pallet {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
                pub enum Call {
                    #[codec(index = 0)]
                    #[doc = "Send a batch of dispatch calls."]
                    #[doc = ""]
                    #[doc = "May be called from any origin except `None`."]
                    #[doc = ""]
                    #[doc = "- `calls`: The calls to be dispatched from the same origin. The number of call must not"]
                    #[doc = "  exceed the constant: `batched_calls_limit` (available in constant metadata)."]
                    #[doc = ""]
                    #[doc = "If origin is root then the calls are dispatched without checking origin filter. (This"]
                    #[doc = "includes bypassing `frame_system::Config::BaseCallFilter`)."]
                    #[doc = ""]
                    #[doc = "## Complexity"]
                    #[doc = "- O(C) where C is the number of calls to be batched."]
                    #[doc = ""]
                    #[doc = "This will return `Ok` in all circumstances. To determine the success of the batch, an"]
                    #[doc = "event is deposited. If a call failed and the batch was interrupted, then the"]
                    #[doc = "`BatchInterrupted` event is deposited, along with the number of successful calls made"]
                    #[doc = "and the error of the failed call. If all were successful, then the `BatchCompleted`"]
                    #[doc = "event is deposited."]
                    batch {
                        calls: ::subxt::alloc::vec::Vec<runtime_types::acuity_runtime::RuntimeCall>,
                    },
                    #[codec(index = 1)]
                    #[doc = "Send a call through an indexed pseudonym of the sender."]
                    #[doc = ""]
                    #[doc = "Filter from origin are passed along. The call will be dispatched with an origin which"]
                    #[doc = "use the same filter as the origin of this call."]
                    #[doc = ""]
                    #[doc = "NOTE: If you need to ensure that any account-based filtering is not honored (i.e."]
                    #[doc = "because you expect `proxy` to have been used prior in the call stack and you do not want"]
                    #[doc = "the call restrictions to apply to any sub-accounts), then use `as_multi_threshold_1`"]
                    #[doc = "in the Multisig pallet instead."]
                    #[doc = ""]
                    #[doc = "NOTE: Prior to version *12, this was called `as_limited_sub`."]
                    #[doc = ""]
                    #[doc = "The dispatch origin for this call must be _Signed_."]
                    as_derivative {
                        index: ::core::primitive::u16,
                        call:
                            ::subxt::alloc::boxed::Box<runtime_types::acuity_runtime::RuntimeCall>,
                    },
                    #[codec(index = 2)]
                    #[doc = "Send a batch of dispatch calls and atomically execute them."]
                    #[doc = "The whole transaction will rollback and fail if any of the calls failed."]
                    #[doc = ""]
                    #[doc = "May be called from any origin except `None`."]
                    #[doc = ""]
                    #[doc = "- `calls`: The calls to be dispatched from the same origin. The number of call must not"]
                    #[doc = "  exceed the constant: `batched_calls_limit` (available in constant metadata)."]
                    #[doc = ""]
                    #[doc = "If origin is root then the calls are dispatched without checking origin filter. (This"]
                    #[doc = "includes bypassing `frame_system::Config::BaseCallFilter`)."]
                    #[doc = ""]
                    #[doc = "## Complexity"]
                    #[doc = "- O(C) where C is the number of calls to be batched."]
                    batch_all {
                        calls: ::subxt::alloc::vec::Vec<runtime_types::acuity_runtime::RuntimeCall>,
                    },
                    #[codec(index = 3)]
                    #[doc = "Dispatches a function call with a provided origin."]
                    #[doc = ""]
                    #[doc = "The dispatch origin for this call must be _Root_."]
                    #[doc = ""]
                    #[doc = "## Complexity"]
                    #[doc = "- O(1)."]
                    dispatch_as {
                        as_origin:
                            ::subxt::alloc::boxed::Box<runtime_types::acuity_runtime::OriginCaller>,
                        call:
                            ::subxt::alloc::boxed::Box<runtime_types::acuity_runtime::RuntimeCall>,
                    },
                    #[codec(index = 4)]
                    #[doc = "Send a batch of dispatch calls."]
                    #[doc = "Unlike `batch`, it allows errors and won't interrupt."]
                    #[doc = ""]
                    #[doc = "May be called from any origin except `None`."]
                    #[doc = ""]
                    #[doc = "- `calls`: The calls to be dispatched from the same origin. The number of call must not"]
                    #[doc = "  exceed the constant: `batched_calls_limit` (available in constant metadata)."]
                    #[doc = ""]
                    #[doc = "If origin is root then the calls are dispatch without checking origin filter. (This"]
                    #[doc = "includes bypassing `frame_system::Config::BaseCallFilter`)."]
                    #[doc = ""]
                    #[doc = "## Complexity"]
                    #[doc = "- O(C) where C is the number of calls to be batched."]
                    force_batch {
                        calls: ::subxt::alloc::vec::Vec<runtime_types::acuity_runtime::RuntimeCall>,
                    },
                    #[codec(index = 5)]
                    #[doc = "Dispatch a function call with a specified weight."]
                    #[doc = ""]
                    #[doc = "This function does not check the weight of the call, and instead allows the"]
                    #[doc = "Root origin to specify the weight of the call."]
                    #[doc = ""]
                    #[doc = "The dispatch origin for this call must be _Root_."]
                    with_weight {
                        call:
                            ::subxt::alloc::boxed::Box<runtime_types::acuity_runtime::RuntimeCall>,
                        weight: runtime_types::sp_weights::weight_v2::Weight,
                    },
                    #[codec(index = 6)]
                    #[doc = "Dispatch a fallback call in the event the main call fails to execute."]
                    #[doc = "May be called from any origin except `None`."]
                    #[doc = ""]
                    #[doc = "This function first attempts to dispatch the `main` call."]
                    #[doc = "If the `main` call fails, the `fallback` is attemted."]
                    #[doc = "if the fallback is successfully dispatched, the weights of both calls"]
                    #[doc = "are accumulated and an event containing the main call error is deposited."]
                    #[doc = ""]
                    #[doc = "In the event of a fallback failure the whole call fails"]
                    #[doc = "with the weights returned."]
                    #[doc = ""]
                    #[doc = "- `main`: The main call to be dispatched. This is the primary action to execute."]
                    #[doc = "- `fallback`: The fallback call to be dispatched in case the `main` call fails."]
                    #[doc = ""]
                    #[doc = "## Dispatch Logic"]
                    #[doc = "- If the origin is `root`, both the main and fallback calls are executed without"]
                    #[doc = "  applying any origin filters."]
                    #[doc = "- If the origin is not `root`, the origin filter is applied to both the `main` and"]
                    #[doc = "  `fallback` calls."]
                    #[doc = ""]
                    #[doc = "## Use Case"]
                    #[doc = "- Some use cases might involve submitting a `batch` type call in either main, fallback"]
                    #[doc = "  or both."]
                    if_else {
                        main:
                            ::subxt::alloc::boxed::Box<runtime_types::acuity_runtime::RuntimeCall>,
                        fallback:
                            ::subxt::alloc::boxed::Box<runtime_types::acuity_runtime::RuntimeCall>,
                    },
                    #[codec(index = 7)]
                    #[doc = "Dispatches a function call with a provided origin."]
                    #[doc = ""]
                    #[doc = "Almost the same as [`Pallet::dispatch_as`] but forwards any error of the inner call."]
                    #[doc = ""]
                    #[doc = "The dispatch origin for this call must be _Root_."]
                    dispatch_as_fallible {
                        as_origin:
                            ::subxt::alloc::boxed::Box<runtime_types::acuity_runtime::OriginCaller>,
                        call:
                            ::subxt::alloc::boxed::Box<runtime_types::acuity_runtime::RuntimeCall>,
                    },
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "The `Error` enum of this pallet."]
                pub enum Error {
                    #[codec(index = 0)]
                    #[doc = "Too many calls batched."]
                    TooManyCalls,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "The `Event` enum of this pallet"]
                pub enum Event {
                    #[codec(index = 0)]
                    #[doc = "Batch of dispatches did not complete fully. Index of first failing dispatch given, as"]
                    #[doc = "well as the error."]
                    BatchInterrupted {
                        index: ::core::primitive::u32,
                        error: runtime_types::sp_runtime::DispatchError,
                    },
                    #[codec(index = 1)]
                    #[doc = "Batch of dispatches completed fully with no error."]
                    BatchCompleted,
                    #[codec(index = 2)]
                    #[doc = "Batch of dispatches completed but has errors."]
                    BatchCompletedWithErrors,
                    #[codec(index = 3)]
                    #[doc = "A single item within a Batch of dispatches has completed with no error."]
                    ItemCompleted,
                    #[codec(index = 4)]
                    #[doc = "A single item within a Batch of dispatches has completed with error."]
                    ItemFailed {
                        error: runtime_types::sp_runtime::DispatchError,
                    },
                    #[codec(index = 5)]
                    #[doc = "A call was dispatched."]
                    DispatchedAs {
                        result:
                            ::core::result::Result<(), runtime_types::sp_runtime::DispatchError>,
                    },
                    #[codec(index = 6)]
                    #[doc = "Main call was dispatched."]
                    IfElseMainSuccess,
                    #[codec(index = 7)]
                    #[doc = "The fallback call was dispatched."]
                    IfElseFallbackCalled {
                        main_error: runtime_types::sp_runtime::DispatchError,
                    },
                }
            }
        }
        pub mod polkadot_parachain_primitives {
            use super::runtime_types;
            pub mod primitives {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct Id(pub ::core::primitive::u32);
            }
        }
        pub mod sp_arithmetic {
            use super::runtime_types;
            pub mod fixed_point {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct FixedU128(pub ::core::primitive::u128);
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub enum ArithmeticError {
                #[codec(index = 0)]
                Underflow,
                #[codec(index = 1)]
                Overflow,
                #[codec(index = 2)]
                DivisionByZero,
            }
        }
        pub mod sp_consensus_aura {
            use super::runtime_types;
            pub mod sr25519 {
                use super::runtime_types;
                pub mod app_sr25519 {
                    use super::runtime_types;
                    #[derive(
                        :: subxt :: ext :: scale_decode :: DecodeAsType,
                        :: subxt :: ext :: scale_encode :: EncodeAsType,
                        Debug,
                    )]
                    #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                    #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                    pub struct Public(pub [::core::primitive::u8; 32usize]);
                }
            }
        }
        pub mod sp_consensus_slots {
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub struct Slot(pub ::core::primitive::u64);
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub struct SlotDuration(pub ::core::primitive::u64);
        }
        pub mod sp_core {
            use super::runtime_types;
            pub mod crypto {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct KeyTypeId(pub [::core::primitive::u8; 4usize]);
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub struct OpaqueMetadata(pub ::subxt::alloc::vec::Vec<::core::primitive::u8>);
        }
        pub mod sp_inherents {
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub struct CheckInherentsResult {
                pub okay: ::core::primitive::bool,
                pub fatal_error: ::core::primitive::bool,
                pub errors: runtime_types::sp_inherents::InherentData,
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub struct InherentData {
                pub data: ::subxt::utils::KeyedVec<
                    [::core::primitive::u8; 8usize],
                    ::subxt::alloc::vec::Vec<::core::primitive::u8>,
                >,
            }
        }
        pub mod sp_runtime {
            use super::runtime_types;
            pub mod generic {
                use super::runtime_types;
                pub mod block {
                    use super::runtime_types;
                    #[derive(
                        :: subxt :: ext :: scale_decode :: DecodeAsType,
                        :: subxt :: ext :: scale_encode :: EncodeAsType,
                        Debug,
                    )]
                    #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                    #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                    pub struct Block<_0, _1> {
                        pub header: _0,
                        pub extrinsics: ::subxt::alloc::vec::Vec<_1>,
                    }
                    #[derive(
                        :: subxt :: ext :: scale_decode :: DecodeAsType,
                        :: subxt :: ext :: scale_encode :: EncodeAsType,
                        Debug,
                    )]
                    #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                    #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                    pub struct LazyBlock<_0, _1> {
                        pub header: _0,
                        pub extrinsics:
                            ::subxt::alloc::vec::Vec<runtime_types::sp_runtime::OpaqueExtrinsic>,
                        #[codec(skip)]
                        pub __ignore: ::core::marker::PhantomData<_1>,
                    }
                }
                pub mod digest {
                    use super::runtime_types;
                    #[derive(
                        :: subxt :: ext :: scale_decode :: DecodeAsType,
                        :: subxt :: ext :: scale_encode :: EncodeAsType,
                        Debug,
                    )]
                    #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                    #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                    pub struct Digest {
                        pub logs: ::subxt::alloc::vec::Vec<
                            runtime_types::sp_runtime::generic::digest::DigestItem,
                        >,
                    }
                    #[derive(
                        :: subxt :: ext :: scale_decode :: DecodeAsType,
                        :: subxt :: ext :: scale_encode :: EncodeAsType,
                        Debug,
                    )]
                    #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                    #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                    pub enum DigestItem {
                        #[codec(index = 6)]
                        PreRuntime(
                            [::core::primitive::u8; 4usize],
                            ::subxt::alloc::vec::Vec<::core::primitive::u8>,
                        ),
                        #[codec(index = 4)]
                        Consensus(
                            [::core::primitive::u8; 4usize],
                            ::subxt::alloc::vec::Vec<::core::primitive::u8>,
                        ),
                        #[codec(index = 5)]
                        Seal(
                            [::core::primitive::u8; 4usize],
                            ::subxt::alloc::vec::Vec<::core::primitive::u8>,
                        ),
                        #[codec(index = 0)]
                        Other(::subxt::alloc::vec::Vec<::core::primitive::u8>),
                        #[codec(index = 8)]
                        RuntimeEnvironmentUpdated,
                    }
                }
                pub mod era {
                    use super::runtime_types;
                    #[derive(
                        :: subxt :: ext :: scale_decode :: DecodeAsType,
                        :: subxt :: ext :: scale_encode :: EncodeAsType,
                        Debug,
                    )]
                    #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                    #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                    pub enum Era {
                        #[codec(index = 0)]
                        Immortal,
                        #[codec(index = 1)]
                        Mortal1(::core::primitive::u8),
                        #[codec(index = 2)]
                        Mortal2(::core::primitive::u8),
                        #[codec(index = 3)]
                        Mortal3(::core::primitive::u8),
                        #[codec(index = 4)]
                        Mortal4(::core::primitive::u8),
                        #[codec(index = 5)]
                        Mortal5(::core::primitive::u8),
                        #[codec(index = 6)]
                        Mortal6(::core::primitive::u8),
                        #[codec(index = 7)]
                        Mortal7(::core::primitive::u8),
                        #[codec(index = 8)]
                        Mortal8(::core::primitive::u8),
                        #[codec(index = 9)]
                        Mortal9(::core::primitive::u8),
                        #[codec(index = 10)]
                        Mortal10(::core::primitive::u8),
                        #[codec(index = 11)]
                        Mortal11(::core::primitive::u8),
                        #[codec(index = 12)]
                        Mortal12(::core::primitive::u8),
                        #[codec(index = 13)]
                        Mortal13(::core::primitive::u8),
                        #[codec(index = 14)]
                        Mortal14(::core::primitive::u8),
                        #[codec(index = 15)]
                        Mortal15(::core::primitive::u8),
                        #[codec(index = 16)]
                        Mortal16(::core::primitive::u8),
                        #[codec(index = 17)]
                        Mortal17(::core::primitive::u8),
                        #[codec(index = 18)]
                        Mortal18(::core::primitive::u8),
                        #[codec(index = 19)]
                        Mortal19(::core::primitive::u8),
                        #[codec(index = 20)]
                        Mortal20(::core::primitive::u8),
                        #[codec(index = 21)]
                        Mortal21(::core::primitive::u8),
                        #[codec(index = 22)]
                        Mortal22(::core::primitive::u8),
                        #[codec(index = 23)]
                        Mortal23(::core::primitive::u8),
                        #[codec(index = 24)]
                        Mortal24(::core::primitive::u8),
                        #[codec(index = 25)]
                        Mortal25(::core::primitive::u8),
                        #[codec(index = 26)]
                        Mortal26(::core::primitive::u8),
                        #[codec(index = 27)]
                        Mortal27(::core::primitive::u8),
                        #[codec(index = 28)]
                        Mortal28(::core::primitive::u8),
                        #[codec(index = 29)]
                        Mortal29(::core::primitive::u8),
                        #[codec(index = 30)]
                        Mortal30(::core::primitive::u8),
                        #[codec(index = 31)]
                        Mortal31(::core::primitive::u8),
                        #[codec(index = 32)]
                        Mortal32(::core::primitive::u8),
                        #[codec(index = 33)]
                        Mortal33(::core::primitive::u8),
                        #[codec(index = 34)]
                        Mortal34(::core::primitive::u8),
                        #[codec(index = 35)]
                        Mortal35(::core::primitive::u8),
                        #[codec(index = 36)]
                        Mortal36(::core::primitive::u8),
                        #[codec(index = 37)]
                        Mortal37(::core::primitive::u8),
                        #[codec(index = 38)]
                        Mortal38(::core::primitive::u8),
                        #[codec(index = 39)]
                        Mortal39(::core::primitive::u8),
                        #[codec(index = 40)]
                        Mortal40(::core::primitive::u8),
                        #[codec(index = 41)]
                        Mortal41(::core::primitive::u8),
                        #[codec(index = 42)]
                        Mortal42(::core::primitive::u8),
                        #[codec(index = 43)]
                        Mortal43(::core::primitive::u8),
                        #[codec(index = 44)]
                        Mortal44(::core::primitive::u8),
                        #[codec(index = 45)]
                        Mortal45(::core::primitive::u8),
                        #[codec(index = 46)]
                        Mortal46(::core::primitive::u8),
                        #[codec(index = 47)]
                        Mortal47(::core::primitive::u8),
                        #[codec(index = 48)]
                        Mortal48(::core::primitive::u8),
                        #[codec(index = 49)]
                        Mortal49(::core::primitive::u8),
                        #[codec(index = 50)]
                        Mortal50(::core::primitive::u8),
                        #[codec(index = 51)]
                        Mortal51(::core::primitive::u8),
                        #[codec(index = 52)]
                        Mortal52(::core::primitive::u8),
                        #[codec(index = 53)]
                        Mortal53(::core::primitive::u8),
                        #[codec(index = 54)]
                        Mortal54(::core::primitive::u8),
                        #[codec(index = 55)]
                        Mortal55(::core::primitive::u8),
                        #[codec(index = 56)]
                        Mortal56(::core::primitive::u8),
                        #[codec(index = 57)]
                        Mortal57(::core::primitive::u8),
                        #[codec(index = 58)]
                        Mortal58(::core::primitive::u8),
                        #[codec(index = 59)]
                        Mortal59(::core::primitive::u8),
                        #[codec(index = 60)]
                        Mortal60(::core::primitive::u8),
                        #[codec(index = 61)]
                        Mortal61(::core::primitive::u8),
                        #[codec(index = 62)]
                        Mortal62(::core::primitive::u8),
                        #[codec(index = 63)]
                        Mortal63(::core::primitive::u8),
                        #[codec(index = 64)]
                        Mortal64(::core::primitive::u8),
                        #[codec(index = 65)]
                        Mortal65(::core::primitive::u8),
                        #[codec(index = 66)]
                        Mortal66(::core::primitive::u8),
                        #[codec(index = 67)]
                        Mortal67(::core::primitive::u8),
                        #[codec(index = 68)]
                        Mortal68(::core::primitive::u8),
                        #[codec(index = 69)]
                        Mortal69(::core::primitive::u8),
                        #[codec(index = 70)]
                        Mortal70(::core::primitive::u8),
                        #[codec(index = 71)]
                        Mortal71(::core::primitive::u8),
                        #[codec(index = 72)]
                        Mortal72(::core::primitive::u8),
                        #[codec(index = 73)]
                        Mortal73(::core::primitive::u8),
                        #[codec(index = 74)]
                        Mortal74(::core::primitive::u8),
                        #[codec(index = 75)]
                        Mortal75(::core::primitive::u8),
                        #[codec(index = 76)]
                        Mortal76(::core::primitive::u8),
                        #[codec(index = 77)]
                        Mortal77(::core::primitive::u8),
                        #[codec(index = 78)]
                        Mortal78(::core::primitive::u8),
                        #[codec(index = 79)]
                        Mortal79(::core::primitive::u8),
                        #[codec(index = 80)]
                        Mortal80(::core::primitive::u8),
                        #[codec(index = 81)]
                        Mortal81(::core::primitive::u8),
                        #[codec(index = 82)]
                        Mortal82(::core::primitive::u8),
                        #[codec(index = 83)]
                        Mortal83(::core::primitive::u8),
                        #[codec(index = 84)]
                        Mortal84(::core::primitive::u8),
                        #[codec(index = 85)]
                        Mortal85(::core::primitive::u8),
                        #[codec(index = 86)]
                        Mortal86(::core::primitive::u8),
                        #[codec(index = 87)]
                        Mortal87(::core::primitive::u8),
                        #[codec(index = 88)]
                        Mortal88(::core::primitive::u8),
                        #[codec(index = 89)]
                        Mortal89(::core::primitive::u8),
                        #[codec(index = 90)]
                        Mortal90(::core::primitive::u8),
                        #[codec(index = 91)]
                        Mortal91(::core::primitive::u8),
                        #[codec(index = 92)]
                        Mortal92(::core::primitive::u8),
                        #[codec(index = 93)]
                        Mortal93(::core::primitive::u8),
                        #[codec(index = 94)]
                        Mortal94(::core::primitive::u8),
                        #[codec(index = 95)]
                        Mortal95(::core::primitive::u8),
                        #[codec(index = 96)]
                        Mortal96(::core::primitive::u8),
                        #[codec(index = 97)]
                        Mortal97(::core::primitive::u8),
                        #[codec(index = 98)]
                        Mortal98(::core::primitive::u8),
                        #[codec(index = 99)]
                        Mortal99(::core::primitive::u8),
                        #[codec(index = 100)]
                        Mortal100(::core::primitive::u8),
                        #[codec(index = 101)]
                        Mortal101(::core::primitive::u8),
                        #[codec(index = 102)]
                        Mortal102(::core::primitive::u8),
                        #[codec(index = 103)]
                        Mortal103(::core::primitive::u8),
                        #[codec(index = 104)]
                        Mortal104(::core::primitive::u8),
                        #[codec(index = 105)]
                        Mortal105(::core::primitive::u8),
                        #[codec(index = 106)]
                        Mortal106(::core::primitive::u8),
                        #[codec(index = 107)]
                        Mortal107(::core::primitive::u8),
                        #[codec(index = 108)]
                        Mortal108(::core::primitive::u8),
                        #[codec(index = 109)]
                        Mortal109(::core::primitive::u8),
                        #[codec(index = 110)]
                        Mortal110(::core::primitive::u8),
                        #[codec(index = 111)]
                        Mortal111(::core::primitive::u8),
                        #[codec(index = 112)]
                        Mortal112(::core::primitive::u8),
                        #[codec(index = 113)]
                        Mortal113(::core::primitive::u8),
                        #[codec(index = 114)]
                        Mortal114(::core::primitive::u8),
                        #[codec(index = 115)]
                        Mortal115(::core::primitive::u8),
                        #[codec(index = 116)]
                        Mortal116(::core::primitive::u8),
                        #[codec(index = 117)]
                        Mortal117(::core::primitive::u8),
                        #[codec(index = 118)]
                        Mortal118(::core::primitive::u8),
                        #[codec(index = 119)]
                        Mortal119(::core::primitive::u8),
                        #[codec(index = 120)]
                        Mortal120(::core::primitive::u8),
                        #[codec(index = 121)]
                        Mortal121(::core::primitive::u8),
                        #[codec(index = 122)]
                        Mortal122(::core::primitive::u8),
                        #[codec(index = 123)]
                        Mortal123(::core::primitive::u8),
                        #[codec(index = 124)]
                        Mortal124(::core::primitive::u8),
                        #[codec(index = 125)]
                        Mortal125(::core::primitive::u8),
                        #[codec(index = 126)]
                        Mortal126(::core::primitive::u8),
                        #[codec(index = 127)]
                        Mortal127(::core::primitive::u8),
                        #[codec(index = 128)]
                        Mortal128(::core::primitive::u8),
                        #[codec(index = 129)]
                        Mortal129(::core::primitive::u8),
                        #[codec(index = 130)]
                        Mortal130(::core::primitive::u8),
                        #[codec(index = 131)]
                        Mortal131(::core::primitive::u8),
                        #[codec(index = 132)]
                        Mortal132(::core::primitive::u8),
                        #[codec(index = 133)]
                        Mortal133(::core::primitive::u8),
                        #[codec(index = 134)]
                        Mortal134(::core::primitive::u8),
                        #[codec(index = 135)]
                        Mortal135(::core::primitive::u8),
                        #[codec(index = 136)]
                        Mortal136(::core::primitive::u8),
                        #[codec(index = 137)]
                        Mortal137(::core::primitive::u8),
                        #[codec(index = 138)]
                        Mortal138(::core::primitive::u8),
                        #[codec(index = 139)]
                        Mortal139(::core::primitive::u8),
                        #[codec(index = 140)]
                        Mortal140(::core::primitive::u8),
                        #[codec(index = 141)]
                        Mortal141(::core::primitive::u8),
                        #[codec(index = 142)]
                        Mortal142(::core::primitive::u8),
                        #[codec(index = 143)]
                        Mortal143(::core::primitive::u8),
                        #[codec(index = 144)]
                        Mortal144(::core::primitive::u8),
                        #[codec(index = 145)]
                        Mortal145(::core::primitive::u8),
                        #[codec(index = 146)]
                        Mortal146(::core::primitive::u8),
                        #[codec(index = 147)]
                        Mortal147(::core::primitive::u8),
                        #[codec(index = 148)]
                        Mortal148(::core::primitive::u8),
                        #[codec(index = 149)]
                        Mortal149(::core::primitive::u8),
                        #[codec(index = 150)]
                        Mortal150(::core::primitive::u8),
                        #[codec(index = 151)]
                        Mortal151(::core::primitive::u8),
                        #[codec(index = 152)]
                        Mortal152(::core::primitive::u8),
                        #[codec(index = 153)]
                        Mortal153(::core::primitive::u8),
                        #[codec(index = 154)]
                        Mortal154(::core::primitive::u8),
                        #[codec(index = 155)]
                        Mortal155(::core::primitive::u8),
                        #[codec(index = 156)]
                        Mortal156(::core::primitive::u8),
                        #[codec(index = 157)]
                        Mortal157(::core::primitive::u8),
                        #[codec(index = 158)]
                        Mortal158(::core::primitive::u8),
                        #[codec(index = 159)]
                        Mortal159(::core::primitive::u8),
                        #[codec(index = 160)]
                        Mortal160(::core::primitive::u8),
                        #[codec(index = 161)]
                        Mortal161(::core::primitive::u8),
                        #[codec(index = 162)]
                        Mortal162(::core::primitive::u8),
                        #[codec(index = 163)]
                        Mortal163(::core::primitive::u8),
                        #[codec(index = 164)]
                        Mortal164(::core::primitive::u8),
                        #[codec(index = 165)]
                        Mortal165(::core::primitive::u8),
                        #[codec(index = 166)]
                        Mortal166(::core::primitive::u8),
                        #[codec(index = 167)]
                        Mortal167(::core::primitive::u8),
                        #[codec(index = 168)]
                        Mortal168(::core::primitive::u8),
                        #[codec(index = 169)]
                        Mortal169(::core::primitive::u8),
                        #[codec(index = 170)]
                        Mortal170(::core::primitive::u8),
                        #[codec(index = 171)]
                        Mortal171(::core::primitive::u8),
                        #[codec(index = 172)]
                        Mortal172(::core::primitive::u8),
                        #[codec(index = 173)]
                        Mortal173(::core::primitive::u8),
                        #[codec(index = 174)]
                        Mortal174(::core::primitive::u8),
                        #[codec(index = 175)]
                        Mortal175(::core::primitive::u8),
                        #[codec(index = 176)]
                        Mortal176(::core::primitive::u8),
                        #[codec(index = 177)]
                        Mortal177(::core::primitive::u8),
                        #[codec(index = 178)]
                        Mortal178(::core::primitive::u8),
                        #[codec(index = 179)]
                        Mortal179(::core::primitive::u8),
                        #[codec(index = 180)]
                        Mortal180(::core::primitive::u8),
                        #[codec(index = 181)]
                        Mortal181(::core::primitive::u8),
                        #[codec(index = 182)]
                        Mortal182(::core::primitive::u8),
                        #[codec(index = 183)]
                        Mortal183(::core::primitive::u8),
                        #[codec(index = 184)]
                        Mortal184(::core::primitive::u8),
                        #[codec(index = 185)]
                        Mortal185(::core::primitive::u8),
                        #[codec(index = 186)]
                        Mortal186(::core::primitive::u8),
                        #[codec(index = 187)]
                        Mortal187(::core::primitive::u8),
                        #[codec(index = 188)]
                        Mortal188(::core::primitive::u8),
                        #[codec(index = 189)]
                        Mortal189(::core::primitive::u8),
                        #[codec(index = 190)]
                        Mortal190(::core::primitive::u8),
                        #[codec(index = 191)]
                        Mortal191(::core::primitive::u8),
                        #[codec(index = 192)]
                        Mortal192(::core::primitive::u8),
                        #[codec(index = 193)]
                        Mortal193(::core::primitive::u8),
                        #[codec(index = 194)]
                        Mortal194(::core::primitive::u8),
                        #[codec(index = 195)]
                        Mortal195(::core::primitive::u8),
                        #[codec(index = 196)]
                        Mortal196(::core::primitive::u8),
                        #[codec(index = 197)]
                        Mortal197(::core::primitive::u8),
                        #[codec(index = 198)]
                        Mortal198(::core::primitive::u8),
                        #[codec(index = 199)]
                        Mortal199(::core::primitive::u8),
                        #[codec(index = 200)]
                        Mortal200(::core::primitive::u8),
                        #[codec(index = 201)]
                        Mortal201(::core::primitive::u8),
                        #[codec(index = 202)]
                        Mortal202(::core::primitive::u8),
                        #[codec(index = 203)]
                        Mortal203(::core::primitive::u8),
                        #[codec(index = 204)]
                        Mortal204(::core::primitive::u8),
                        #[codec(index = 205)]
                        Mortal205(::core::primitive::u8),
                        #[codec(index = 206)]
                        Mortal206(::core::primitive::u8),
                        #[codec(index = 207)]
                        Mortal207(::core::primitive::u8),
                        #[codec(index = 208)]
                        Mortal208(::core::primitive::u8),
                        #[codec(index = 209)]
                        Mortal209(::core::primitive::u8),
                        #[codec(index = 210)]
                        Mortal210(::core::primitive::u8),
                        #[codec(index = 211)]
                        Mortal211(::core::primitive::u8),
                        #[codec(index = 212)]
                        Mortal212(::core::primitive::u8),
                        #[codec(index = 213)]
                        Mortal213(::core::primitive::u8),
                        #[codec(index = 214)]
                        Mortal214(::core::primitive::u8),
                        #[codec(index = 215)]
                        Mortal215(::core::primitive::u8),
                        #[codec(index = 216)]
                        Mortal216(::core::primitive::u8),
                        #[codec(index = 217)]
                        Mortal217(::core::primitive::u8),
                        #[codec(index = 218)]
                        Mortal218(::core::primitive::u8),
                        #[codec(index = 219)]
                        Mortal219(::core::primitive::u8),
                        #[codec(index = 220)]
                        Mortal220(::core::primitive::u8),
                        #[codec(index = 221)]
                        Mortal221(::core::primitive::u8),
                        #[codec(index = 222)]
                        Mortal222(::core::primitive::u8),
                        #[codec(index = 223)]
                        Mortal223(::core::primitive::u8),
                        #[codec(index = 224)]
                        Mortal224(::core::primitive::u8),
                        #[codec(index = 225)]
                        Mortal225(::core::primitive::u8),
                        #[codec(index = 226)]
                        Mortal226(::core::primitive::u8),
                        #[codec(index = 227)]
                        Mortal227(::core::primitive::u8),
                        #[codec(index = 228)]
                        Mortal228(::core::primitive::u8),
                        #[codec(index = 229)]
                        Mortal229(::core::primitive::u8),
                        #[codec(index = 230)]
                        Mortal230(::core::primitive::u8),
                        #[codec(index = 231)]
                        Mortal231(::core::primitive::u8),
                        #[codec(index = 232)]
                        Mortal232(::core::primitive::u8),
                        #[codec(index = 233)]
                        Mortal233(::core::primitive::u8),
                        #[codec(index = 234)]
                        Mortal234(::core::primitive::u8),
                        #[codec(index = 235)]
                        Mortal235(::core::primitive::u8),
                        #[codec(index = 236)]
                        Mortal236(::core::primitive::u8),
                        #[codec(index = 237)]
                        Mortal237(::core::primitive::u8),
                        #[codec(index = 238)]
                        Mortal238(::core::primitive::u8),
                        #[codec(index = 239)]
                        Mortal239(::core::primitive::u8),
                        #[codec(index = 240)]
                        Mortal240(::core::primitive::u8),
                        #[codec(index = 241)]
                        Mortal241(::core::primitive::u8),
                        #[codec(index = 242)]
                        Mortal242(::core::primitive::u8),
                        #[codec(index = 243)]
                        Mortal243(::core::primitive::u8),
                        #[codec(index = 244)]
                        Mortal244(::core::primitive::u8),
                        #[codec(index = 245)]
                        Mortal245(::core::primitive::u8),
                        #[codec(index = 246)]
                        Mortal246(::core::primitive::u8),
                        #[codec(index = 247)]
                        Mortal247(::core::primitive::u8),
                        #[codec(index = 248)]
                        Mortal248(::core::primitive::u8),
                        #[codec(index = 249)]
                        Mortal249(::core::primitive::u8),
                        #[codec(index = 250)]
                        Mortal250(::core::primitive::u8),
                        #[codec(index = 251)]
                        Mortal251(::core::primitive::u8),
                        #[codec(index = 252)]
                        Mortal252(::core::primitive::u8),
                        #[codec(index = 253)]
                        Mortal253(::core::primitive::u8),
                        #[codec(index = 254)]
                        Mortal254(::core::primitive::u8),
                        #[codec(index = 255)]
                        Mortal255(::core::primitive::u8),
                    }
                }
                pub mod header {
                    use super::runtime_types;
                    #[derive(
                        :: subxt :: ext :: scale_decode :: DecodeAsType,
                        :: subxt :: ext :: scale_encode :: EncodeAsType,
                        Debug,
                    )]
                    #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                    #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                    pub struct Header<_0> {
                        pub parent_hash: ::subxt::utils::H256,
                        #[codec(compact)]
                        pub number: _0,
                        pub state_root: ::subxt::utils::H256,
                        pub extrinsics_root: ::subxt::utils::H256,
                        pub digest: runtime_types::sp_runtime::generic::digest::Digest,
                    }
                }
            }
            pub mod proving_trie {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub enum TrieError {
                    #[codec(index = 0)]
                    InvalidStateRoot,
                    #[codec(index = 1)]
                    IncompleteDatabase,
                    #[codec(index = 2)]
                    ValueAtIncompleteKey,
                    #[codec(index = 3)]
                    DecoderError,
                    #[codec(index = 4)]
                    InvalidHash,
                    #[codec(index = 5)]
                    DuplicateKey,
                    #[codec(index = 6)]
                    ExtraneousNode,
                    #[codec(index = 7)]
                    ExtraneousValue,
                    #[codec(index = 8)]
                    ExtraneousHashReference,
                    #[codec(index = 9)]
                    InvalidChildReference,
                    #[codec(index = 10)]
                    ValueMismatch,
                    #[codec(index = 11)]
                    IncompleteProof,
                    #[codec(index = 12)]
                    RootMismatch,
                    #[codec(index = 13)]
                    DecodeError,
                }
            }
            pub mod traits {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct BlakeTwo256;
            }
            pub mod transaction_validity {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub enum InvalidTransaction {
                    #[codec(index = 0)]
                    Call,
                    #[codec(index = 1)]
                    Payment,
                    #[codec(index = 2)]
                    Future,
                    #[codec(index = 3)]
                    Stale,
                    #[codec(index = 4)]
                    BadProof,
                    #[codec(index = 5)]
                    AncientBirthBlock,
                    #[codec(index = 6)]
                    ExhaustsResources,
                    #[codec(index = 7)]
                    Custom(::core::primitive::u8),
                    #[codec(index = 8)]
                    BadMandatory,
                    #[codec(index = 9)]
                    MandatoryValidation,
                    #[codec(index = 10)]
                    BadSigner,
                    #[codec(index = 11)]
                    IndeterminateImplicit,
                    #[codec(index = 12)]
                    UnknownOrigin,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub enum TransactionSource {
                    #[codec(index = 0)]
                    InBlock,
                    #[codec(index = 1)]
                    Local,
                    #[codec(index = 2)]
                    External,
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub enum TransactionValidityError {
                    #[codec(index = 0)]
                    Invalid(runtime_types::sp_runtime::transaction_validity::InvalidTransaction),
                    #[codec(index = 1)]
                    Unknown(runtime_types::sp_runtime::transaction_validity::UnknownTransaction),
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub enum UnknownTransaction {
                    #[codec(index = 0)]
                    CannotLookup,
                    #[codec(index = 1)]
                    NoUnsignedValidator,
                    #[codec(index = 2)]
                    Custom(::core::primitive::u8),
                }
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct ValidTransaction {
                    pub priority: ::core::primitive::u64,
                    pub requires:
                        ::subxt::alloc::vec::Vec<::subxt::alloc::vec::Vec<::core::primitive::u8>>,
                    pub provides:
                        ::subxt::alloc::vec::Vec<::subxt::alloc::vec::Vec<::core::primitive::u8>>,
                    pub longevity: ::core::primitive::u64,
                    pub propagate: ::core::primitive::bool,
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub enum DispatchError {
                #[codec(index = 0)]
                Other,
                #[codec(index = 1)]
                CannotLookup,
                #[codec(index = 2)]
                BadOrigin,
                #[codec(index = 3)]
                Module(runtime_types::sp_runtime::ModuleError),
                #[codec(index = 4)]
                ConsumerRemaining,
                #[codec(index = 5)]
                NoProviders,
                #[codec(index = 6)]
                TooManyConsumers,
                #[codec(index = 7)]
                Token(runtime_types::sp_runtime::TokenError),
                #[codec(index = 8)]
                Arithmetic(runtime_types::sp_arithmetic::ArithmeticError),
                #[codec(index = 9)]
                Transactional(runtime_types::sp_runtime::TransactionalError),
                #[codec(index = 10)]
                Exhausted,
                #[codec(index = 11)]
                Corruption,
                #[codec(index = 12)]
                Unavailable,
                #[codec(index = 13)]
                RootNotAllowed,
                #[codec(index = 14)]
                Trie(runtime_types::sp_runtime::proving_trie::TrieError),
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub enum ExtrinsicInclusionMode {
                #[codec(index = 0)]
                AllExtrinsics,
                #[codec(index = 1)]
                OnlyInherents,
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub struct ModuleError {
                pub index: ::core::primitive::u8,
                pub error: [::core::primitive::u8; 4usize],
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub enum MultiSignature {
                #[codec(index = 0)]
                Ed25519([::core::primitive::u8; 64usize]),
                #[codec(index = 1)]
                Sr25519([::core::primitive::u8; 64usize]),
                #[codec(index = 2)]
                Ecdsa([::core::primitive::u8; 65usize]),
                #[codec(index = 3)]
                Eth([::core::primitive::u8; 65usize]),
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub struct OpaqueExtrinsic(pub ::subxt::alloc::vec::Vec<::core::primitive::u8>);
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub enum TokenError {
                #[codec(index = 0)]
                FundsUnavailable,
                #[codec(index = 1)]
                OnlyProvider,
                #[codec(index = 2)]
                BelowMinimum,
                #[codec(index = 3)]
                CannotCreate,
                #[codec(index = 4)]
                UnknownAsset,
                #[codec(index = 5)]
                Frozen,
                #[codec(index = 6)]
                Unsupported,
                #[codec(index = 7)]
                CannotCreateHold,
                #[codec(index = 8)]
                NotExpendable,
                #[codec(index = 9)]
                Blocked,
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub enum TransactionalError {
                #[codec(index = 0)]
                LimitReached,
                #[codec(index = 1)]
                NoLayer,
            }
        }
        pub mod sp_session {
            use super::runtime_types;
            pub mod runtime_api {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct OpaqueGeneratedSessionKeys {
                    pub keys: ::subxt::alloc::vec::Vec<::core::primitive::u8>,
                    pub proof: ::subxt::alloc::vec::Vec<::core::primitive::u8>,
                }
            }
        }
        pub mod sp_version {
            use super::runtime_types;
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub struct RuntimeVersion {
                pub spec_name: ::subxt::alloc::string::String,
                pub impl_name: ::subxt::alloc::string::String,
                pub authoring_version: ::core::primitive::u32,
                pub spec_version: ::core::primitive::u32,
                pub impl_version: ::core::primitive::u32,
                pub apis: ::subxt::alloc::vec::Vec<(
                    [::core::primitive::u8; 8usize],
                    ::core::primitive::u32,
                )>,
                pub transaction_version: ::core::primitive::u32,
                pub system_version: ::core::primitive::u8,
            }
        }
        pub mod sp_weights {
            use super::runtime_types;
            pub mod weight_v2 {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                pub struct Weight {
                    #[codec(compact)]
                    pub ref_time: ::core::primitive::u64,
                    #[codec(compact)]
                    pub proof_size: ::core::primitive::u64,
                }
            }
            #[derive(
                :: subxt :: ext :: scale_decode :: DecodeAsType,
                :: subxt :: ext :: scale_encode :: EncodeAsType,
                Debug,
            )]
            #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
            #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
            pub struct RuntimeDbWeight {
                pub read: ::core::primitive::u64,
                pub write: ::core::primitive::u64,
            }
        }
        pub mod staging_parachain_info {
            use super::runtime_types;
            pub mod pallet {
                use super::runtime_types;
                #[derive(
                    :: subxt :: ext :: scale_decode :: DecodeAsType,
                    :: subxt :: ext :: scale_encode :: EncodeAsType,
                    Debug,
                )]
                #[decode_as_type(crate_path = ":: subxt :: ext :: scale_decode")]
                #[encode_as_type(crate_path = ":: subxt :: ext :: scale_encode")]
                #[doc = "Contains a variant per dispatchable extrinsic that this pallet has."]
                pub enum Call {}
            }
        }
    }
}
