//! The `rpc` module implements the Solana RPC interface.
#[cfg(feature = "dev-context-only-utils")]

use std::ops::Deref;
use std::sync::{Arc, RwLock};

use solana_accounts_db::epoch_accounts_hash::EpochAccountsHash;
use solana_poh::poh_recorder::PohRecorder;
use solana_pubkey::Pubkey;

use solana_runtime::bank_forks::BankForks;
use solana_runtime::commitment::BlockCommitmentCache;
use solana_runtime::bank::Bank;
use jsonrpc_core::{Result, Error};
use solana_rpc_client_api::custom_error::RpcCustomError;
use solana_sdk::hash::Hash;
use solana_runtime::accounts_background_service::{AbsRequestSender, SnapshotRequestKind};
use crate::rpc::JsonRpcRequestProcessor;

// Allow automatic forwarding of method calls to the base implementation
impl Deref for TestValidatorJsonRpcRequestProcessor {
    type Target = JsonRpcRequestProcessor;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

pub struct TestValidatorJsonRpcRequestProcessor {
    pub base: JsonRpcRequestProcessor,
    pub poh_recorder: Arc<RwLock<PohRecorder>>,
    pub bank_forks: Arc<RwLock<BankForks>>,
    pub block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
}

impl TestValidatorJsonRpcRequestProcessor {
    pub fn warp_slot(&self, target_slot: u64) -> Result<()> {
        let mut bank_forks = self.bank_forks.write().unwrap();
        let bank = bank_forks.working_bank();
        let poh_recorder = self.poh_recorder.write().unwrap();
        poh_recorder.pause();
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
        poh_recorder.resume();
        let mut w_block_commitment_cache = self.block_commitment_cache.write().unwrap();
        w_block_commitment_cache.set_all_slots(target_slot, target_slot);
        Ok(())
    }
}