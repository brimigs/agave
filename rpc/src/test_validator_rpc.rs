//! The `rpc` module implements the Solana RPC interface.

use std::ops::Deref;
use solana_accounts_db::epoch_accounts_hash::EpochAccountsHash;
use solana_runtime::accounts_background_service::{AbsRequestSender, SnapshotRequestKind};
use {
    crate::{
        filter::filter_allows, max_slots::MaxSlots,
        optimistically_confirmed_bank_tracker::OptimisticallyConfirmedBank,
        parsed_token_accounts::*, rpc_cache::LargestAccountsCache, rpc_health::*,
    },
    base64::{prelude::BASE64_STANDARD, Engine},
    bincode::{config::Options, serialize},
    crossbeam_channel::{unbounded, Receiver, Sender},
    jsonrpc_core::{
        futures::future::{self, FutureExt, OptionFuture},
        types::error,
        BoxFuture, Error, Metadata, Result,
    },
    jsonrpc_derive::rpc,
    solana_account_decoder::{
        encode_ui_account,
        parse_account_data::SplTokenAdditionalData,
        parse_token::{is_known_spl_token_id, token_amount_to_ui_amount_v2, UiTokenAmount},
        UiAccount, UiAccountEncoding, UiDataSliceConfig, MAX_BASE58_BYTES,
    },
    solana_accounts_db::{
        accounts::AccountAddressFilter,
        accounts_index::{AccountIndex, AccountSecondaryIndexes, IndexKey, ScanConfig, ScanResult},
    },
    solana_client::connection_cache::Protocol,
    solana_entry::entry::Entry,
    solana_faucet::faucet::request_airdrop_transaction,
    solana_feature_set as feature_set,
    solana_gossip::cluster_info::ClusterInfo,
    solana_inline_spl::{
        token::{SPL_TOKEN_ACCOUNT_MINT_OFFSET, SPL_TOKEN_ACCOUNT_OWNER_OFFSET},
        token_2022::{self, ACCOUNTTYPE_ACCOUNT},
    },
    solana_metrics::inc_new_counter_info,
    solana_perf::packet::PACKET_DATA_SIZE,
    solana_rpc_client_api::{
        config::*,
        custom_error::RpcCustomError,
        filter::{Memcmp, RpcFilterType},
        request::{
            TokenAccountsFilter, DELINQUENT_VALIDATOR_SLOT_DISTANCE,
            MAX_GET_CONFIRMED_BLOCKS_RANGE, MAX_GET_CONFIRMED_SIGNATURES_FOR_ADDRESS2_LIMIT,
            MAX_GET_PROGRAM_ACCOUNT_FILTERS, MAX_GET_SIGNATURE_STATUSES_QUERY_ITEMS,
            MAX_GET_SLOT_LEADERS, MAX_MULTIPLE_ACCOUNTS,
            MAX_RPC_VOTE_ACCOUNT_INFO_EPOCH_CREDITS_HISTORY, NUM_LARGEST_ACCOUNTS,
        },
        response::{Response as RpcResponse, *},
    },
    solana_runtime::{
        bank::{Bank, TransactionSimulationResult},
        bank_forks::BankForks,
        commitment::{BlockCommitmentArray, BlockCommitmentCache},
        installed_scheduler_pool::BankWithScheduler,
        non_circulating_supply::{calculate_non_circulating_supply, NonCirculatingSupply},
        prioritization_fee_cache::PrioritizationFeeCache,
        snapshot_config::SnapshotConfig,
        snapshot_utils,
        verify_precompiles::verify_precompiles,
    },
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_sdk::{
        account::{AccountSharedData, ReadableAccount},
        clock::{Slot, UnixTimestamp, MAX_PROCESSING_AGE},
        commitment_config::{CommitmentConfig, CommitmentLevel},
        epoch_info::EpochInfo,
        epoch_rewards_hasher::EpochRewardsHasher,
        epoch_schedule::EpochSchedule,
        exit::Exit,
        hash::Hash,
        message::SanitizedMessage,
        pubkey::{Pubkey, PUBKEY_BYTES},
        signature::{Keypair, Signature, Signer},
        system_instruction,
        transaction::{
            self, AddressLoader, MessageHash, SanitizedTransaction, TransactionError,
            VersionedTransaction, MAX_TX_ACCOUNT_LOCKS,
        },
        transaction_context::TransactionAccount,
    },
    solana_send_transaction_service::send_transaction_service::TransactionInfo,
    solana_stake_program,
    solana_storage_bigtable::Error as StorageError,
    solana_transaction_status::{
        map_inner_instructions, BlockEncodingOptions, ConfirmedBlock,
        ConfirmedTransactionStatusWithSignature, ConfirmedTransactionWithStatusMeta,
        EncodedConfirmedTransactionWithStatusMeta, Reward, RewardType, Rewards,
        TransactionBinaryEncoding, TransactionConfirmationStatus, TransactionStatus,
        UiConfirmedBlock, UiTransactionEncoding,
    },
    solana_vote_program::vote_state::MAX_LOCKOUT_HISTORY,
    spl_token_2022::{
        extension::{
            interest_bearing_mint::InterestBearingConfig, BaseStateWithExtensions,
            StateWithExtensions,
        },
        solana_program::program_pack::Pack,
        state::{Account as TokenAccount, Mint},
    },
    std::{
        any::type_name,
        cmp::{max, min, Reverse},
        collections::{BinaryHeap, HashMap, HashSet},
        convert::TryFrom,
        net::SocketAddr,
        str::FromStr,
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, RwLock,
        },
        time::Duration,
    },
    tokio::runtime::Runtime,
};
use solana_poh::poh_recorder::PohRecorder;
use crate::rpc::JsonRpcRequestProcessor;

// Allow automatic forwarding of method calls to the base implementation
impl Deref for TestValidatorJsonRpcRequestProcessor {
    type Target = JsonRpcRequestProcessor;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}
pub struct TestValidatorJsonRpcRequestProcessor {
    base: JsonRpcRequestProcessor,
    poh_recorder: Arc<RwLock<PohRecorder>>,
    bank_forks: Arc<RwLock<BankForks>>,
    block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
}

impl TestValidatorJsonRpcRequestProcessor {
    pub fn warp_slot(&self, target_slot: u64) -> Result<()> {
        let mut bank_forks = self.bank_forks.write().unwrap();
        let bank = bank_forks.working_bank();

        let working_slot = bank.slot();
        if target_slot <= working_slot {
            return Err(Error::from(RpcCustomError::InvalidWarpSlot));
        }

        let pre_warp_slot = target_slot - 1;
        let warp_bank = if pre_warp_slot == working_slot {
            bank.freeze();
            bank
        } else {
            bank_forks
                .insert(Bank::warp_from_parent(
                    bank,
                    &Pubkey::default(),
                    pre_warp_slot,
                    solana_accounts_db::accounts_db::CalcAccountsHashDataSource::IndexForTests,
                ))
                .clone_without_scheduler()
        };

        let (snapshot_request_sender, snapshot_request_receiver) = crossbeam_channel::unbounded();
        let abs_request_sender = AbsRequestSender::new(snapshot_request_sender);

        bank_forks
            .set_root(pre_warp_slot, &abs_request_sender, Some(pre_warp_slot))
            .unwrap();

        snapshot_request_receiver
            .try_iter()
            .filter(|snapshot_request| {
                snapshot_request.request_kind == SnapshotRequestKind::EpochAccountsHash
            })
            .for_each(|snapshot_request| {
                snapshot_request
                    .snapshot_root_bank
                    .rc
                    .accounts
                    .accounts_db
                    .epoch_accounts_hash_manager
                    .set_valid(
                        EpochAccountsHash::new(Hash::new_unique()),
                        snapshot_request.snapshot_root_bank.slot(),
                    )
            });

        bank_forks.insert(Bank::new_from_parent(
            warp_bank,
            &Pubkey::default(),
            target_slot,
        ));

        let mut w_block_commitment_cache = self.block_commitment_cache.write().unwrap();

        w_block_commitment_cache.set_all_slots(target_slot, target_slot);

        Ok(())
    }
}
