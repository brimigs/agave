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
use solana_sdk::{account::AccountSharedData, clock::Clock, hash::Hash, program_pack::Pack};
use solana_runtime::accounts_background_service::{AbsRequestSender, SnapshotRequestKind};
use crate::rpc::JsonRpcRequestProcessor;
use spl_token::state::{Account as TokenAccount, AccountState};
use solana_sdk::account::ReadableAccount;
// use solana_core::consensus::progress_map::{ForkProgress, ProgressMap};

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
    // pub progress: Arc<RwLock<ProgressMap>>
}

impl TestValidatorJsonRpcRequestProcessor {
    pub fn warp_slot_impl(&self, target_slot: u64) -> Result<()> {
        let mut bank_forks = self.bank_forks.write().unwrap();
        let bank = bank_forks.working_bank();
        let mut poh_recorder = self.poh_recorder.write().unwrap();
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
        // let mut progress = self.progress.write().unwrap();
        // progress.insert(
        //     target_slot,
        //     ForkProgress::new_from_bank(
        //         &bank,
        //         &Pubkey::default(), // validator identity
        //         &Pubkey::default(), // vote account
        //         Some(pre_warp_slot), // prev_leader_slot
        //         0, // num_blocks_on_fork
        //         0, // num_dropped_blocks_on_fork
        //     )
        // );
        poh_recorder.reset(bank_forks.get(target_slot).unwrap(), None);
        poh_recorder.resume();
        let mut w_block_commitment_cache = self.block_commitment_cache.write().unwrap();
        w_block_commitment_cache.set_all_slots(target_slot, target_slot);
        Ok(())
    }

    // pub fn update_token_balance(&self, target_slot: u64) -> Result<()> {
    //     let mut bank_forks = self.bank_forks.write().unwrap();
    //     let bank = bank_forks.working_bank();
    //     let mut poh_recorder = self.poh_recorder.write().unwrap();
    //     poh_recorder.pause();
        
    // }

    pub fn set_account(&self, address: &Pubkey, account: &AccountSharedData) {
        let bank_forks = self.bank_forks.read().unwrap();
        let bank = bank_forks.working_bank();
        bank.store_account(address, account);
    }

    pub fn update_token_account_impl(
        &self,
        token_account: Option<&Pubkey>,
        mint: Option<&Pubkey>,
        owner: Option<&Pubkey>,
        amount: Option<u64>,
    ) -> Result<Pubkey> {
        use solana_sdk::account::ReadableAccount;
        
        let bank_forks = self.bank_forks.read().unwrap();
        let bank = bank_forks.working_bank();
        
        // If token_account is not provided, we need mint to create a new account
        let token_account_pubkey = match token_account {
            Some(pubkey) => *pubkey,
            None => {
                // If no token_account provided, mint must be provided
                let mint_pubkey = mint.ok_or_else(|| 
                    Error::invalid_params("Either token_account or mint must be provided")
                )?;
                
                // Create a new token account with a random address
                let new_token_account = Pubkey::new_unique();
                
                // Owner must be provided when creating a new account
                let owner_pubkey = owner.ok_or_else(|| 
                    Error::invalid_params("Owner must be provided when creating a new token account")
                )?;
                
                // Amount defaults to 0 for new accounts if not specified
                let token_amount = amount.unwrap_or(0);
                
                // Create a new token account
                let account_data_size = TokenAccount::get_packed_len();
                let token_state = TokenAccount {
                    mint: *mint_pubkey,
                    owner: *owner_pubkey,
                    amount: token_amount,
                    delegate: None.into(),
                    state: AccountState::Initialized,
                    is_native: None.into(),
                    delegated_amount: 0,
                    close_authority: None.into(),
                };
                
                let mut data = vec![0; account_data_size];
                TokenAccount::pack(token_state, &mut data).map_err(|e| {
                    Error::invalid_params(format!("Failed to pack token account data: {}", e))
                })?;
                
                let mut account = AccountSharedData::new(
                    bank.get_minimum_balance_for_rent_exemption(account_data_size),
                    account_data_size,
                    &spl_token::id(),
                );
                account.set_data(data);
                
                bank.store_account(&new_token_account, &account);
                
                return Ok(new_token_account);
            }
        };
        
        // Get the existing account if it exists
        let existing_account = bank.get_account(&token_account_pubkey).ok_or_else(|| {
            Error::invalid_params(format!("Token account {} not found", token_account_pubkey))
        })?;
        
        // Check if it's a token account
        if existing_account.owner() != &spl_token::id() {
            return Err(Error::invalid_params(format!(
                "Account {} is not a token account", token_account_pubkey
            )));
        }
        
        // Try to unpack the existing token account data
        let mut token_data = TokenAccount::unpack(&existing_account.data())
            .map_err(|e| Error::invalid_params(format!("Invalid token account data: {}", e)))?;
        
        // Update fields if provided
        if let Some(owner_pubkey) = owner {
            token_data.owner = *owner_pubkey;
        }
        
        if let Some(mint_pubkey) = mint {
            token_data.mint = *mint_pubkey;
        }
        
        if let Some(token_amount) = amount {
            token_data.amount = token_amount;
        }
        
        // Pack the updated token account data
        let account_data_size = TokenAccount::get_packed_len();
        let mut data = vec![0; account_data_size];
        TokenAccount::pack(token_data, &mut data).map_err(|e| {
            Error::invalid_params(format!("Failed to pack token account data: {}", e))
        })?;
        
        // Create the updated account
        let mut account = AccountSharedData::new(
            existing_account.lamports(),
            account_data_size,
            &spl_token::id(),
        );
        account.set_data(data);
        
        // Store the updated account
        bank.store_account(&token_account_pubkey, &account);
        
        Ok(token_account_pubkey)
    }
    
    pub fn clone_account_from_cluster_impl(
        &self,
        address: &Pubkey,
        url: Option<&str>,
    ) -> Result<()> {
        use solana_client::rpc_client::RpcClient;  
        use solana_sdk::account::{ReadableAccount, WritableAccount};
        
        // Default to mainnet-beta if no URL is provided
        let url = url.unwrap_or("https://api.mainnet-beta.solana.com");
        
        // Create a blocking client for simplicity
        let client = RpcClient::new(url.to_string());
        
        // Fetch the account data from the remote cluster
        let account = client.get_account(address)
            .map_err(|err| Error::invalid_params(format!(
                "Failed to fetch account from {}: {}", url, err
            )))?;
        
        // Convert to AccountSharedData
        let mut account_data = AccountSharedData::new(
            account.lamports(),
            account.data().len(),
            account.owner(),
        ); 
        account_data.set_data(account.data().to_vec());
        account_data.set_executable(account.executable());
        account_data.set_rent_epoch(account.rent_epoch());
        
        // Store the account in the test validator
        let bank_forks = self.bank_forks.read().unwrap();
        let bank = bank_forks.working_bank();
        bank.store_account(address, &account_data);
        
        Ok(())
    }

    pub fn set_clock_impl(&self, slot: u64, unix_timestamp: i64, epoch: Option<u64>) {
        let bank_forks = self.bank_forks.read().unwrap();
        let bank = bank_forks.working_bank();
        
        // Calculate epoch if not provided
        let epoch_schedule = bank.epoch_schedule();
        let epoch = epoch.unwrap_or_else(|| epoch_schedule.get_epoch(slot));
        let leader_schedule_epoch = epoch_schedule.get_leader_schedule_epoch(slot);
        
        // Determine epoch_start_timestamp
        let epoch_start_slot = epoch_schedule.get_first_slot_in_epoch(epoch);
        let epoch_start_timestamp = if epoch_start_slot == slot {
            unix_timestamp
        } else {
            // If we're not at the start of the epoch, use the existing epoch_start_timestamp
            bank.clock().epoch_start_timestamp
        };
        
        // Create the new Clock sysvar
        let clock = Clock {
            slot,
            epoch_start_timestamp,
            epoch,
            leader_schedule_epoch,
            unix_timestamp,
        };
        
        // Set the sysvar
        bank.set_sysvar_for_tests(&clock);
    }

    // pub fn update_token_balance(
    //     &self,
    //     token_account: &Pubkey,
    //     mint: &Pubkey,
    //     owner: &Pubkey,
    //     amount: u64,
    // ) -> Result<()> {
    //     use solana_sdk::program_pack::Pack;
    //     use solana_inline_spl::token::state::{Account as TokenAccount, Mint as TokenMint};
        
    //     let bank_forks = self.bank_forks.read().unwrap();
    //     let bank = bank_forks.working_bank();
        
    //     // First, ensure the mint exists or create it
    //     let mint_account = bank.get_account(mint);
    //     if mint_account.is_none() {
    //         // Create a default mint with decimals=6 (like USDC)
    //         let mint_data_size = TokenMint::get_packed_len();
    //         let mut mint_data = vec![0; mint_data_size];
    //         let mint_state = TokenMint {
    //             mint_authority: solana_program::program_option::COption::Some(*owner),
    //             supply: amount,
    //             decimals: 6, // USDC has 6 decimals
    //             is_initialized: true,
    //             freeze_authority: solana_program::program_option::COption::Some(*owner),
    //         };
    //         TokenMint::pack(mint_state, &mut mint_data).map_err(|e| {
    //             Error::invalid_params(format!("Failed to pack mint data: {}", e))
    //         })?;
            
    //         let mint_account = AccountSharedData::new(
    //             bank.get_minimum_balance_for_rent_exemption(mint_data_size),
    //             mint_data.len(),
    //             &solana_inline_spl::token::id(),
    //         );
            
    //         let mut mint_account = mint_account.set_data(mint_data);
    //         bank.store_account(mint, &mint_account);
    //     }
        
    //     // Now update or create the token account
    //     let account_data_size = TokenAccount::get_packed_len();
    //     let mut account_data = vec![0; account_data_size];
        
    //     // If the account already exists, preserve its state except for the amount
    //     let token_account_data = if let Some(existing_account) = bank.get_account(token_account) {
    //         if existing_account.owner() == &solana_inline_spl::token::id() {
    //             let mut token_state = TokenAccount::unpack(&existing_account.data())
    //                 .map_err(|e| Error::invalid_params(format!("Invalid token account data: {}", e)))?;
    //             token_state.amount = amount;
    //             token_state
    //         } else {
    //             // Create a new token account state
    //             TokenAccount {
    //                 mint: *mint,
    //                 owner: *owner,
    //                 amount,
    //                 delegate: solana_program::program_option::COption::None,
    //                 state: solana_inline_spl::token::state::AccountState::Initialized,
    //                 is_native: solana_program::program_option::COption::None,
    //                 delegated_amount: 0,
    //                 close_authority: solana_program::program_option::COption::None,
    //             }
    //         }
    //     } else {
    //         // Create a new token account state
    //         TokenAccount {
    //             mint: *mint,
    //             owner: *owner,
    //             amount,
    //             delegate: solana_program::program_option::COption::None,
    //             state: solana_inline_spl::token::state::AccountState::Initialized,
    //             is_native: solana_program::program_option::COption::None,
    //             delegated_amount: 0,
    //             close_authority: solana_program::program_option::COption::None,
    //         }
    //     };
        
    //     TokenAccount::pack(token_account_data, &mut account_data).map_err(|e| {
    //         Error::invalid_params(format!("Failed to pack token account data: {}", e))
    //     })?;
        
    //     let token_account_obj = AccountSharedData::new(
    //         bank.get_minimum_balance_for_rent_exemption(account_data_size),
    //         account_data.len(),
    //         &solana_inline_spl::token::id(),
    //     );
        
    //     let token_account_obj = token_account_obj.set_data(account_data);
    //     bank.store_account(token_account, &token_account_obj);
        
    //     Ok(())
    // }
}
