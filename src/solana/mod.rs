//! Solana RPC client for on-chain integration.

use amos_core::{AmosError, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use std::str::FromStr;
use std::sync::Arc;

/// PDA seeds for treasury program accounts (from amos-treasury/constants.rs)
pub const TREASURY_CONFIG: &[u8] = b"treasury_config";
pub const STAKE_RECORD: &[u8] = b"stake_record";
pub const DISTRIBUTION: &[u8] = b"distribution";
pub const HOLDER_POOL: &[u8] = b"holder_pool";

/// Wrapper around Solana RPC client.
pub struct SolanaClient {
    rpc: Arc<RpcClient>,
    rpc_url: String,
    /// Treasury program ID
    pub treasury_program_id: Pubkey,
    /// Governance program ID
    pub governance_program_id: Pubkey,
    /// Bounty program ID
    pub bounty_program_id: Pubkey,
    /// Optional authority keypair for signing transactions
    pub authority_keypair: Option<Arc<Keypair>>,
}

impl SolanaClient {
    /// Create a new Solana client connected to the given RPC endpoint.
    ///
    /// # Arguments
    /// * `rpc_url` - Solana RPC endpoint URL
    /// * `treasury_program_id` - Program ID for the treasury program
    /// * `governance_program_id` - Program ID for the governance program
    /// * `bounty_program_id` - Program ID for the bounty program
    pub fn new(
        rpc_url: &str,
        treasury_program_id: &str,
        governance_program_id: &str,
        bounty_program_id: &str,
    ) -> Result<Self> {
        let rpc = RpcClient::new_with_commitment(
            rpc_url.to_string(),
            CommitmentConfig::confirmed(),
        );

        let treasury_program_id = Pubkey::from_str(treasury_program_id)
            .map_err(|e| AmosError::SolanaRpc(format!("Invalid treasury program ID: {}", e)))?;
        let governance_program_id = Pubkey::from_str(governance_program_id)
            .map_err(|e| AmosError::SolanaRpc(format!("Invalid governance program ID: {}", e)))?;
        let bounty_program_id = Pubkey::from_str(bounty_program_id)
            .map_err(|e| AmosError::SolanaRpc(format!("Invalid bounty program ID: {}", e)))?;

        Ok(Self {
            rpc: Arc::new(rpc),
            rpc_url: rpc_url.to_string(),
            treasury_program_id,
            governance_program_id,
            bounty_program_id,
            authority_keypair: None,
        })
    }

    /// Set the authority keypair for signing transactions.
    pub fn with_authority(mut self, keypair: Keypair) -> Self {
        self.authority_keypair = Some(Arc::new(keypair));
        self
    }

    /// Health check: verify RPC is reachable.
    pub async fn health_check(&self) -> Result<()> {
        // Spawn blocking since RPC client is sync
        let rpc_url = self.rpc_url.clone();
        tokio::task::spawn_blocking(move || {
            let rpc = RpcClient::new_with_commitment(
                rpc_url,
                CommitmentConfig::confirmed(),
            );
            rpc.get_health()
                .map_err(|e| AmosError::SolanaRpc(format!("Health check failed: {}", e)))
        })
        .await
        .map_err(|e| AmosError::Internal(format!("Tokio join error: {}", e)))??;

        Ok(())
    }

    /// Derive a PDA for the treasury config account.
    ///
    /// PDA = findProgramAddress([b"treasury_config"], treasury_program_id)
    pub fn derive_treasury_config_pda(&self) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[TREASURY_CONFIG], &self.treasury_program_id)
    }

    /// Derive a PDA for a stake record account.
    ///
    /// PDA = findProgramAddress([b"stake_record", wallet_pubkey], treasury_program_id)
    pub fn derive_stake_record_pda(&self, wallet: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[STAKE_RECORD, wallet.as_ref()],
            &self.treasury_program_id,
        )
    }

    /// Get the treasury state from on-chain program.
    ///
    /// Fetches the treasury config PDA and deserializes the account data.
    ///
    /// # Account Layout (Anchor)
    /// - Bytes 0-7: Anchor account discriminator (8 bytes)
    /// - Bytes 8-15: total_supply (u64, little-endian)
    /// - Bytes 16-23: circulating_supply (u64, little-endian)
    /// - Bytes 24-31: treasury_balance (u64, little-endian)
    /// - Bytes 32-39: holder_pool_usdc (u64, little-endian)
    /// - Bytes 40-47: total_staked (u64, little-endian)
    /// - Bytes 48-55: current_emission_rate (u64, little-endian)
    /// - Bytes 56-63: last_emission_day (u64, little-endian)
    pub async fn get_treasury_state(&self) -> Result<TreasuryState> {
        let (treasury_config_pda, _bump) = self.derive_treasury_config_pda();

        let rpc = self.rpc.clone();
        let account = tokio::task::spawn_blocking(move || {
            rpc.get_account(&treasury_config_pda)
        })
        .await
        .map_err(|e| AmosError::Internal(format!("Tokio join error: {}", e)))?
        .map_err(|e| AmosError::SolanaRpc(format!("Failed to fetch treasury config: {}", e)))?;

        // Verify the account has enough data (8-byte discriminator + 7 * 8 bytes)
        const MIN_SIZE: usize = 8 + 7 * 8;
        if account.data.len() < MIN_SIZE {
            return Err(AmosError::SolanaRpc(format!(
                "Treasury config account data too small: {} bytes, expected at least {}",
                account.data.len(),
                MIN_SIZE
            )));
        }

        // Skip the 8-byte Anchor discriminator and deserialize the fields
        let data = &account.data[8..];

        Ok(TreasuryState {
            total_supply: u64::from_le_bytes(data[0..8].try_into().unwrap()),
            circulating_supply: u64::from_le_bytes(data[8..16].try_into().unwrap()),
            treasury_balance: u64::from_le_bytes(data[16..24].try_into().unwrap()),
            holder_pool_usdc: u64::from_le_bytes(data[24..32].try_into().unwrap()),
            total_staked: u64::from_le_bytes(data[32..40].try_into().unwrap()),
            current_emission_rate: u64::from_le_bytes(data[40..48].try_into().unwrap()),
            last_emission_day: u64::from_le_bytes(data[48..56].try_into().unwrap()),
        })
    }

    /// Get stake record for a wallet address.
    ///
    /// Returns None if the account doesn't exist (user has never staked).
    ///
    /// # Account Layout (Anchor)
    /// - Bytes 0-7: Anchor account discriminator (8 bytes)
    /// - Bytes 8-39: wallet (Pubkey, 32 bytes)
    /// - Bytes 40-47: amount (u64, little-endian)
    /// - Bytes 48-55: staked_at (i64 unix timestamp, little-endian)
    /// - Bytes 56-63: last_decay_at (i64 unix timestamp, little-endian)
    /// - Byte 64: vault_tier (u8)
    /// - Byte 65: has_delegation (u8, 0 = no, 1 = yes)
    /// - Bytes 66-97: delegated_to (Pubkey, 32 bytes, only valid if has_delegation = 1)
    pub async fn get_stake_record(&self, wallet: &str) -> Result<Option<StakeRecord>> {
        let wallet_pubkey = Pubkey::from_str(wallet)
            .map_err(|e| AmosError::SolanaRpc(format!("Invalid wallet address: {}", e)))?;

        let (stake_record_pda, _bump) = self.derive_stake_record_pda(&wallet_pubkey);

        let rpc = self.rpc.clone();
        let account_result = tokio::task::spawn_blocking(move || {
            rpc.get_account(&stake_record_pda)
        })
        .await
        .map_err(|e| AmosError::Internal(format!("Tokio join error: {}", e)))?;

        // If account doesn't exist, return None
        let account = match account_result {
            Ok(acc) => acc,
            Err(_) => return Ok(None),
        };

        // Verify the account has enough data
        const MIN_SIZE: usize = 8 + 32 + 8 + 8 + 8 + 1 + 1 + 32;
        if account.data.len() < MIN_SIZE {
            return Err(AmosError::SolanaRpc(format!(
                "Stake record account data too small: {} bytes, expected at least {}",
                account.data.len(),
                MIN_SIZE
            )));
        }

        // Skip the 8-byte Anchor discriminator and deserialize the fields
        let data = &account.data[8..];

        let amount = u64::from_le_bytes(data[32..40].try_into().unwrap());
        let staked_at = i64::from_le_bytes(data[40..48].try_into().unwrap());
        let last_decay_at = i64::from_le_bytes(data[48..56].try_into().unwrap());
        let vault_tier = data[56];
        let has_delegation = data[57];

        let delegated_to = if has_delegation == 1 {
            let delegated_pubkey = Pubkey::try_from(&data[58..90])
                .map_err(|e| AmosError::SolanaRpc(format!("Invalid delegated pubkey: {}", e)))?;
            Some(delegated_pubkey.to_string())
        } else {
            None
        };

        Ok(Some(StakeRecord {
            wallet: wallet.to_string(),
            amount,
            staked_at: DateTime::from_timestamp(staked_at, 0)
                .ok_or_else(|| AmosError::SolanaRpc("Invalid staked_at timestamp".to_string()))?,
            last_decay_at: DateTime::from_timestamp(last_decay_at, 0)
                .ok_or_else(|| AmosError::SolanaRpc("Invalid last_decay_at timestamp".to_string()))?,
            vault_tier,
            delegated_to,
        }))
    }

    /// Get all active governance proposals.
    ///
    /// Uses `get_program_accounts()` with a filter to find all proposal accounts.
    ///
    /// # Account Layout (Anchor)
    /// - Bytes 0-7: Anchor account discriminator (8 bytes)
    /// - Bytes 8-15: proposal_id (u64, little-endian)
    /// - Bytes 16-47: proposer (Pubkey, 32 bytes)
    /// - Bytes 48-79: title_hash ([u8; 32])
    /// - Bytes 80-87: voting_starts_at (i64, little-endian)
    /// - Bytes 88-95: voting_ends_at (i64, little-endian)
    /// - Bytes 96-103: votes_for (u64, little-endian)
    /// - Bytes 104-111: votes_against (u64, little-endian)
    /// - Byte 112: executed (u8, 0 = false, 1 = true)
    pub async fn get_governance_proposals(&self) -> Result<Vec<OnChainProposal>> {
        let governance_program_id = self.governance_program_id;
        let rpc = self.rpc.clone();

        // We filter for accounts with the proposal discriminator
        // For Anchor, the discriminator is hash("account:Proposal")[..8]
        // Since we don't have the exact discriminator, we'll fetch all accounts
        // and filter by size (proposals should be a specific size)
        let config = RpcProgramAccountsConfig {
            filters: None, // Could add size filter if known
            account_config: RpcAccountInfoConfig {
                encoding: None,
                commitment: Some(CommitmentConfig::confirmed()),
                data_slice: None,
                min_context_slot: None,
            },
            with_context: None,
            sort_results: None,
        };

        let accounts = tokio::task::spawn_blocking(move || {
            rpc.get_program_accounts_with_config(&governance_program_id, config)
        })
        .await
        .map_err(|e| AmosError::Internal(format!("Tokio join error: {}", e)))?
        .map_err(|e| AmosError::SolanaRpc(format!("Failed to fetch governance proposals: {}", e)))?;

        let mut proposals = Vec::new();

        for (_pubkey, account) in accounts {
            // Verify the account has enough data
            const MIN_SIZE: usize = 8 + 8 + 32 + 32 + 8 + 8 + 8 + 8 + 1;
            if account.data.len() < MIN_SIZE {
                continue; // Skip accounts that are too small
            }

            // Skip the 8-byte Anchor discriminator and deserialize the fields
            let data = &account.data[8..];

            let proposal_id = u64::from_le_bytes(data[0..8].try_into().unwrap());
            let proposer_bytes: [u8; 32] = data[8..40].try_into().unwrap();
            let proposer = Pubkey::try_from(&proposer_bytes[..])
                .map_err(|e| AmosError::SolanaRpc(format!("Invalid proposer pubkey: {}", e)))?;
            let title_hash: [u8; 32] = data[40..72].try_into().unwrap();
            let voting_starts_at = i64::from_le_bytes(data[72..80].try_into().unwrap());
            let voting_ends_at = i64::from_le_bytes(data[80..88].try_into().unwrap());
            let votes_for = u64::from_le_bytes(data[88..96].try_into().unwrap());
            let votes_against = u64::from_le_bytes(data[96..104].try_into().unwrap());
            let executed = data[104] == 1;

            proposals.push(OnChainProposal {
                proposal_id,
                proposer: proposer.to_string(),
                title_hash,
                voting_starts_at,
                voting_ends_at,
                votes_for,
                votes_against,
                executed,
            });
        }

        Ok(proposals)
    }

    /// Submit a bounty proof transaction.
    ///
    /// Builds and signs a transaction to submit bounty proof on-chain.
    /// Requires that `authority_keypair` is set on the client.
    ///
    /// # Instruction Layout
    /// - Instruction discriminator: 8 bytes (Anchor discriminator for submit_proof)
    /// - bounty_id: u64 (8 bytes, little-endian)
    /// - contribution_type: u8 (1 byte)
    /// - points: u64 (8 bytes, little-endian)
    /// - evidence_hash: [u8; 32] (32 bytes)
    ///
    /// # Accounts
    /// 0. [writable, signer] authority
    /// 1. [writable] bounty_proof_pda
    /// 2. [] contributor (wallet receiving the bounty)
    /// 3. [] bounty_program
    /// 4. [] system_program
    pub async fn submit_bounty_proof(
        &self,
        bounty_id: u64,
        contributor: &str,
        evidence_hash: [u8; 32],
    ) -> Result<String> {
        let authority = self.authority_keypair.as_ref()
            .ok_or_else(|| AmosError::SolanaRpc("Authority keypair not set".to_string()))?;

        let contributor_pubkey = Pubkey::from_str(contributor)
            .map_err(|e| AmosError::SolanaRpc(format!("Invalid contributor address: {}", e)))?;

        // Derive the bounty proof PDA
        // PDA = findProgramAddress([b"bounty_proof", bounty_id.to_le_bytes()], bounty_program_id)
        let (bounty_proof_pda, _bump) = Pubkey::find_program_address(
            &[b"bounty_proof", &bounty_id.to_le_bytes()],
            &self.bounty_program_id,
        );

        // Build instruction data
        // For this example, we'll use a placeholder discriminator
        // In a real implementation, this would be the hash of "global:submit_proof"
        let mut instruction_data = Vec::new();
        // Placeholder discriminator (8 bytes) - would need actual value from IDL
        instruction_data.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11]);
        instruction_data.extend_from_slice(&bounty_id.to_le_bytes());
        instruction_data.push(1); // contribution_type: code contribution
        instruction_data.extend_from_slice(&100u64.to_le_bytes()); // points
        instruction_data.extend_from_slice(&evidence_hash);

        let instruction = Instruction {
            program_id: self.bounty_program_id,
            accounts: vec![
                AccountMeta::new(authority.pubkey(), true),
                AccountMeta::new(bounty_proof_pda, false),
                AccountMeta::new_readonly(contributor_pubkey, false),
                AccountMeta::new_readonly(self.bounty_program_id, false),
                AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            ],
            data: instruction_data,
        };

        let rpc = self.rpc.clone();
        let authority_pubkey = authority.pubkey();

        // Get recent blockhash
        let recent_blockhash = tokio::task::spawn_blocking(move || {
            rpc.get_latest_blockhash()
        })
        .await
        .map_err(|e| AmosError::Internal(format!("Tokio join error: {}", e)))?
        .map_err(|e| AmosError::SolanaRpc(format!("Failed to get recent blockhash: {}", e)))?;

        // Build and sign transaction
        let message = Message::new(&[instruction], Some(&authority_pubkey));
        let mut transaction = Transaction::new_unsigned(message);
        transaction.sign(&[authority], recent_blockhash);

        // Send transaction
        let rpc = self.rpc.clone();
        let signature = tokio::task::spawn_blocking(move || {
            rpc.send_and_confirm_transaction(&transaction)
        })
        .await
        .map_err(|e| AmosError::Internal(format!("Tokio join error: {}", e)))?
        .map_err(|e| AmosError::SolanaRpc(format!("Failed to submit bounty proof: {}", e)))?;

        Ok(signature.to_string())
    }
}

// PPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPP
// ON-CHAIN DATA TYPES
// PPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPP

/// Treasury program state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreasuryState {
    pub total_supply: u64,
    pub circulating_supply: u64,
    pub treasury_balance: u64,
    pub holder_pool_usdc: u64,
    pub total_staked: u64,
    pub current_emission_rate: u64,
    pub last_emission_day: u64,
}

/// Individual stake record from on-chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StakeRecord {
    pub wallet: String,
    pub amount: u64,
    pub staked_at: DateTime<Utc>,
    pub last_decay_at: DateTime<Utc>,
    pub vault_tier: u8, // 0 = none, 1 = bronze, 2 = silver, 3 = gold, 4 = permanent
    pub delegated_to: Option<String>,
}

/// On-chain governance proposal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnChainProposal {
    pub proposal_id: u64,
    pub proposer: String,
    pub title_hash: [u8; 32],
    pub voting_starts_at: i64,
    pub voting_ends_at: i64,
    pub votes_for: u64,
    pub votes_against: u64,
    pub executed: bool,
}

/// Bounty submission proof.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BountyProof {
    pub bounty_id: u64,
    pub contributor: String,
    pub contribution_type: u8,
    pub points: u64,
    pub evidence_hash: [u8; 32],
    pub reviewer: String,
    pub submitted_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solana_client_can_be_created() {
        let client = SolanaClient::new(
            "https://api.devnet.solana.com",
            "11111111111111111111111111111111", // placeholder program ID
            "11111111111111111111111111111111",
            "11111111111111111111111111111111",
        );
        assert!(client.is_ok());
    }

    #[test]
    fn test_derive_treasury_config_pda() {
        let client = SolanaClient::new(
            "https://api.devnet.solana.com",
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", // SPL Token program as example
            "11111111111111111111111111111111",
            "11111111111111111111111111111111",
        )
        .unwrap();

        let (pda, bump) = client.derive_treasury_config_pda();

        // Verify the PDA derivation is deterministic
        let (pda2, bump2) = client.derive_treasury_config_pda();
        assert_eq!(pda, pda2);
        assert_eq!(bump, bump2);

        // Verify bump is valid (0-255)
        assert!(bump <= 255);
    }

    #[test]
    fn test_derive_stake_record_pda() {
        let client = SolanaClient::new(
            "https://api.devnet.solana.com",
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
            "11111111111111111111111111111111",
            "11111111111111111111111111111111",
        )
        .unwrap();

        let wallet = Pubkey::new_unique();
        let (pda, bump) = client.derive_stake_record_pda(&wallet);

        // Verify the PDA derivation is deterministic
        let (pda2, bump2) = client.derive_stake_record_pda(&wallet);
        assert_eq!(pda, pda2);
        assert_eq!(bump, bump2);

        // Verify different wallets produce different PDAs
        let wallet2 = Pubkey::new_unique();
        let (pda3, _) = client.derive_stake_record_pda(&wallet2);
        assert_ne!(pda, pda3);
    }

    #[test]
    fn test_with_authority() {
        let keypair = Keypair::new();
        let pubkey = keypair.pubkey();

        let client = SolanaClient::new(
            "https://api.devnet.solana.com",
            "11111111111111111111111111111111",
            "11111111111111111111111111111111",
            "11111111111111111111111111111111",
        )
        .unwrap()
        .with_authority(keypair);

        assert!(client.authority_keypair.is_some());
        assert_eq!(client.authority_keypair.as_ref().unwrap().pubkey(), pubkey);
    }
}
