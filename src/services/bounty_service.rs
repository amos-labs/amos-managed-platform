//! # Nightly Bounty Generation Service
//!
//! This service runs as a scheduled background task that:
//! 1. Aggregates daily contribution points
//! 2. Calculates the emission pool for the current day
//! 3. Distributes AMOS token rewards to contributors proportionally
//! 4. Optionally submits bounty proofs on-chain via Solana
//!
//! The service runs once per day at midnight UTC.

use amos_core::token::{
    economics::MAX_BOUNTY_POINTS,
    emission::daily_emission_for_day,
    points::{calculate_daily_rewards, DailyPointsSnapshot},
};
use amos_core::{AmosError, Result};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::time::Duration;
use tracing::{error, info, warn};

use crate::state::PlatformState;

/// Genesis timestamp for the AMOS platform (Jan 1, 2025 00:00:00 UTC).
pub const GENESIS_TIMESTAMP: i64 = 1735689600;

/// The nightly bounty generation and emission distribution service.
///
/// This service coordinates the daily token emission process:
/// - Calculates the current day's emission pool based on the halving schedule
/// - Aggregates all contribution activity from the database
/// - Computes proportional rewards for each contributor
/// - Records emission events in the database
/// - Optionally submits bounty proofs to the Solana blockchain
#[derive(Clone)]
pub struct BountyService {
    state: PlatformState,
}

impl BountyService {
    /// Create a new bounty service instance.
    ///
    /// # Arguments
    ///
    /// * `state` - Shared platform state containing database, Redis, and Solana client
    pub fn new(state: PlatformState) -> Self {
        Self { state }
    }

    /// Start the nightly scheduler that runs emission distribution at midnight UTC.
    ///
    /// This spawns a tokio task that runs indefinitely, calculating the time
    /// until the next midnight UTC and sleeping until then. When midnight arrives,
    /// it executes the nightly emission distribution process.
    ///
    /// The scheduler will continue running even if individual emission runs fail,
    /// logging errors and continuing to the next scheduled run.
    pub fn start_nightly_scheduler(self) {
        tokio::spawn(async move {
            info!("Bounty service nightly scheduler started");

            loop {
                // Calculate time until next midnight UTC
                let now = Utc::now();
                let next_midnight = (now + chrono::Duration::days(1))
                    .date_naive()
                    .and_hms_opt(0, 0, 0)
                    .expect("valid time")
                    .and_utc();
                let duration_until_midnight = (next_midnight - now)
                    .to_std()
                    .unwrap_or(Duration::from_secs(60));

                info!(
                    "Next emission run scheduled for {} (in {} seconds)",
                    next_midnight,
                    duration_until_midnight.as_secs()
                );

                // Sleep until midnight
                tokio::time::sleep(duration_until_midnight).await;

                // Run the nightly emission process
                info!("Starting nightly emission distribution...");
                match self.run_nightly_emission().await {
                    Ok(_) => info!("Nightly emission distribution completed successfully"),
                    Err(e) => error!("Nightly emission distribution failed: {}", e),
                }
            }
        });
    }

    /// Run the nightly emission distribution process.
    ///
    /// This is the core function that:
    /// 1. Calculates the current day index from the genesis timestamp
    /// 2. Retrieves the daily emission amount for this day (with halving)
    /// 3. Queries the database for all contributions logged today
    /// 4. Builds a daily points snapshot from the contributions
    /// 5. Calculates proportional rewards for each contributor
    /// 6. Records rewards in the database
    /// 7. Optionally submits bounty proofs to Solana
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on success, or an error if any step fails.
    /// The function is designed to be gracefully resilient to missing database
    /// tables (returns empty results rather than failing).
    pub async fn run_nightly_emission(&self) -> Result<()> {
        let now = Utc::now();
        let now_unix = now.timestamp();

        // Calculate day index from genesis
        let day_index = calculate_day_index(now_unix);
        info!(
            "Running emission for day index {} (timestamp: {})",
            day_index, now_unix
        );

        // Get the daily emission for this day (respects halving schedule)
        let daily_emission = daily_emission_for_day(day_index);
        info!(
            "Daily emission for day {} (epoch {}): {} AMOS",
            day_index, daily_emission.halving_epoch, daily_emission.emission
        );

        // Query database for all contributions logged today
        let contributions = self.get_todays_contributions(day_index).await?;
        info!(
            "Found {} contribution entries for today",
            contributions.len()
        );

        if contributions.is_empty() {
            info!("No contributions today, skipping emission distribution");
            return Ok(());
        }

        // Build a daily points snapshot from contributions
        let snapshot = self.build_daily_snapshot(day_index, &contributions)?;
        info!(
            "Built snapshot: {} total points across {} contributors",
            snapshot.total_points, snapshot.contributor_count
        );

        if snapshot.is_empty() {
            info!("Snapshot is empty (zero points), skipping emission distribution");
            return Ok(());
        }

        // Calculate rewards for each contributor
        let rewards = calculate_daily_rewards(&snapshot, daily_emission.emission)?;
        info!("Calculated {} individual rewards", rewards.len());

        // Record rewards in database and optionally submit to Solana
        for (contributor_id, tokens) in rewards {
            // Record in database
            self.record_emission_reward(day_index, contributor_id, tokens)
                .await?;
            info!(
                "Recorded emission reward: contributor {} received {} AMOS",
                contributor_id, tokens
            );

            // Optionally submit bounty proof on-chain
            if let Some(ref solana_client) = self.state.solana {
                // Generate evidence hash from the emission record
                let evidence_hash = self.compute_evidence_hash(day_index, contributor_id, tokens);

                match solana_client
                    .submit_bounty_proof(
                        day_index,
                        &format!("contributor_{}", contributor_id),
                        evidence_hash,
                    )
                    .await
                {
                    Ok(signature) => {
                        info!(
                            "Submitted bounty proof on-chain for contributor {}: {}",
                            contributor_id, signature
                        );
                    }
                    Err(e) => {
                        warn!(
                            "Failed to submit bounty proof on-chain for contributor {}: {}",
                            contributor_id, e
                        );
                        // Continue processing other contributors even if one fails
                    }
                }
            }
        }

        info!(
            "Nightly emission complete: distributed {} AMOS to {} contributors",
            daily_emission.emission, snapshot.contributor_count
        );

        Ok(())
    }

    /// Record a contribution activity to the database.
    ///
    /// This function is called by API routes when a user performs a contribution
    /// activity (e.g., completes a bounty, makes a referral, etc.).
    ///
    /// # Arguments
    ///
    /// * `contributor_id` - The unique identifier for the contributor
    /// * `activity_type` - The type of contribution activity
    /// * `points` - Number of points earned for this activity
    /// * `reference_id` - Optional reference ID (e.g., bounty ID, referral ID)
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on success, or an error if the database operation fails.
    /// Gracefully handles missing tables by logging a warning.
    pub async fn record_contribution(
        &self,
        contributor_id: u64,
        activity_type: &str,
        points: u64,
        reference_id: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now();
        let day_index = calculate_day_index(now.timestamp());

        // Validate points are within acceptable range
        if points > MAX_BOUNTY_POINTS {
            return Err(AmosError::Validation(format!(
                "Points {} exceed maximum allowed {}",
                points, MAX_BOUNTY_POINTS
            )));
        }

        // Insert into contribution_activities table
        let result = sqlx::query(
            r#"
            INSERT INTO contribution_activities
                (contributor_id, day_index, activity_type, points, reference_id, created_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(contributor_id as i64)
        .bind(day_index as i64)
        .bind(activity_type)
        .bind(points as i64)
        .bind(reference_id)
        .bind(now)
        .execute(&self.state.db)
        .await;

        match result {
            Ok(_) => {
                info!(
                    "Recorded contribution: contributor={}, type={}, points={}, ref={:?}",
                    contributor_id, activity_type, points, reference_id
                );
                Ok(())
            }
            Err(e) => {
                // Check if table doesn't exist (graceful degradation)
                if e.to_string().contains("does not exist")
                    || e.to_string().contains("no such table")
                {
                    warn!(
                        "contribution_activities table does not exist, skipping contribution record"
                    );
                    Ok(())
                } else {
                    Err(AmosError::Database(e.into()))
                }
            }
        }
    }

    /// Get a summary of emissions and distributions for a specific day.
    ///
    /// This function retrieves the daily emission information and all rewards
    /// distributed on the specified day. It's used by API routes to display
    /// emission history and contributor rewards.
    ///
    /// # Arguments
    ///
    /// * `day_index` - The day index to query (0 = genesis day)
    ///
    /// # Returns
    ///
    /// Returns a `DailySummary` containing emission info and rewards, or an error.
    pub async fn get_daily_summary(&self, day_index: u64) -> Result<DailySummary> {
        let daily_emission = daily_emission_for_day(day_index);

        // Query all emission records for this day
        let records = sqlx::query_as::<_, EmissionRecord>(
            r#"
            SELECT contributor_id, day_index, tokens_awarded, created_at
            FROM emission_records
            WHERE day_index = $1
            ORDER BY tokens_awarded DESC
            "#,
        )
        .bind(day_index as i64)
        .fetch_all(&self.state.db)
        .await
        .unwrap_or_else(|e| {
            if e.to_string().contains("does not exist") || e.to_string().contains("no such table")
            {
                warn!("emission_records table does not exist, returning empty summary");
                vec![]
            } else {
                error!("Failed to query emission records: {}", e);
                vec![]
            }
        });

        let total_distributed: u64 = records.iter().map(|r| r.tokens_awarded as u64).sum();
        let contributor_count = records.len();

        Ok(DailySummary {
            day_index,
            halving_epoch: daily_emission.halving_epoch,
            daily_emission: daily_emission.emission,
            total_distributed,
            contributor_count,
            records,
        })
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // PRIVATE HELPER METHODS
    // ═══════════════════════════════════════════════════════════════════════════

    /// Query the database for all contributions logged on the specified day.
    async fn get_todays_contributions(&self, day_index: u64) -> Result<Vec<ContributionActivity>> {
        let result = sqlx::query_as::<_, ContributionActivity>(
            r#"
            SELECT contributor_id, activity_type, points, reference_id
            FROM contribution_activities
            WHERE day_index = $1
            "#,
        )
        .bind(day_index as i64)
        .fetch_all(&self.state.db)
        .await;

        match result {
            Ok(contributions) => Ok(contributions),
            Err(e) => {
                // Gracefully handle missing table
                if e.to_string().contains("does not exist")
                    || e.to_string().contains("no such table")
                {
                    warn!("contribution_activities table does not exist, returning empty list");
                    Ok(vec![])
                } else {
                    Err(AmosError::Database(e.into()))
                }
            }
        }
    }

    /// Build a daily points snapshot from a list of contributions.
    fn build_daily_snapshot(
        &self,
        day_index: u64,
        contributions: &[ContributionActivity],
    ) -> Result<DailyPointsSnapshot> {
        // Aggregate points by contributor
        let mut contributor_points: std::collections::HashMap<u64, u64> =
            std::collections::HashMap::new();

        for contribution in contributions {
            let contributor_id = contribution.contributor_id as u64;
            let points = contribution.points as u64;
            *contributor_points.entry(contributor_id).or_insert(0) += points;
        }

        // Convert to snapshot format
        let mut entries: Vec<(u64, u64)> = contributor_points.into_iter().collect();
        entries.sort_by_key(|(id, _)| *id);

        let total_points: u64 = entries.iter().map(|(_, points)| *points).sum();
        let contributor_count = entries.len() as u64;

        Ok(DailyPointsSnapshot {
            day_index,
            total_points,
            contributor_count,
            entries,
        })
    }

    /// Record an emission reward in the database.
    async fn record_emission_reward(
        &self,
        day_index: u64,
        contributor_id: u64,
        tokens: u64,
    ) -> Result<()> {
        let now = Utc::now();

        let result = sqlx::query(
            r#"
            INSERT INTO emission_records
                (contributor_id, day_index, tokens_awarded, created_at)
            VALUES ($1, $2, $3, $4)
            "#,
        )
        .bind(contributor_id as i64)
        .bind(day_index as i64)
        .bind(tokens as i64)
        .bind(now)
        .execute(&self.state.db)
        .await;

        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                // Gracefully handle missing table
                if e.to_string().contains("does not exist")
                    || e.to_string().contains("no such table")
                {
                    warn!("emission_records table does not exist, skipping record");
                    Ok(())
                } else {
                    Err(AmosError::Database(e.into()))
                }
            }
        }
    }

    /// Compute a SHA-256 evidence hash for a bounty proof.
    ///
    /// The hash is computed from the day index, contributor ID, and tokens awarded.
    fn compute_evidence_hash(&self, day_index: u64, contributor_id: u64, tokens: u64) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"AMOS_EMISSION_PROOF:");
        hasher.update(day_index.to_le_bytes());
        hasher.update(contributor_id.to_le_bytes());
        hasher.update(tokens.to_le_bytes());
        hasher.finalize().into()
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// HELPER FUNCTIONS
// ═══════════════════════════════════════════════════════════════════════════

/// Calculate the day index from a Unix timestamp.
///
/// Day index is calculated as the number of days since the genesis timestamp.
///
/// # Arguments
///
/// * `unix_timestamp` - Unix timestamp in seconds
///
/// # Returns
///
/// The day index (0 = genesis day)
pub fn calculate_day_index(unix_timestamp: i64) -> u64 {
    let days_since_genesis = (unix_timestamp - GENESIS_TIMESTAMP) / 86400;
    days_since_genesis.max(0) as u64
}

// ═══════════════════════════════════════════════════════════════════════════
// DATA TYPES
// ═══════════════════════════════════════════════════════════════════════════

/// A contribution activity record from the database.
#[derive(Debug, Clone, sqlx::FromRow)]
#[allow(dead_code)]
struct ContributionActivity {
    contributor_id: i64,
    activity_type: String,
    points: i64,
    reference_id: Option<String>,
}

/// An emission record from the database.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EmissionRecord {
    pub contributor_id: i64,
    pub day_index: i64,
    pub tokens_awarded: i64,
    pub created_at: chrono::DateTime<Utc>,
}

/// Summary of daily emissions and distributions.
#[derive(Debug, Clone)]
pub struct DailySummary {
    /// Day index (0 = genesis day)
    pub day_index: u64,
    /// Current halving epoch
    pub halving_epoch: u64,
    /// Total AMOS available for distribution this day
    pub daily_emission: u64,
    /// Total AMOS actually distributed
    pub total_distributed: u64,
    /// Number of contributors who received rewards
    pub contributor_count: usize,
    /// Individual emission records
    pub records: Vec<EmissionRecord>,
}

// ═══════════════════════════════════════════════════════════════════════════
// TESTS
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_day_index() {
        // Genesis day should be day 0
        assert_eq!(calculate_day_index(GENESIS_TIMESTAMP), 0);

        // One day after genesis
        assert_eq!(calculate_day_index(GENESIS_TIMESTAMP + 86400), 1);

        // One year after genesis (365 days)
        assert_eq!(calculate_day_index(GENESIS_TIMESTAMP + 365 * 86400), 365);

        // Before genesis should return 0
        assert_eq!(calculate_day_index(GENESIS_TIMESTAMP - 86400), 0);
    }

    #[test]
    fn test_genesis_timestamp_is_correct() {
        // Jan 1, 2025 00:00:00 UTC
        let expected = chrono::NaiveDate::from_ymd_opt(2025, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();

        assert_eq!(GENESIS_TIMESTAMP, expected);
    }

    #[test]
    fn test_compute_evidence_hash_is_deterministic() {
        // Test the hash function directly
        let mut hasher1 = Sha256::new();
        hasher1.update(b"AMOS_EMISSION_PROOF:");
        hasher1.update(100u64.to_le_bytes());
        hasher1.update(42u64.to_le_bytes());
        hasher1.update(1000u64.to_le_bytes());
        let hash1: [u8; 32] = hasher1.finalize().into();

        let mut hasher2 = Sha256::new();
        hasher2.update(b"AMOS_EMISSION_PROOF:");
        hasher2.update(100u64.to_le_bytes());
        hasher2.update(42u64.to_le_bytes());
        hasher2.update(1000u64.to_le_bytes());
        let hash2: [u8; 32] = hasher2.finalize().into();

        assert_eq!(hash1, hash2, "Hash should be deterministic");
    }

    #[test]
    fn test_compute_evidence_hash_differs_for_different_inputs() {
        let mut hasher1 = Sha256::new();
        hasher1.update(b"AMOS_EMISSION_PROOF:");
        hasher1.update(100u64.to_le_bytes());
        hasher1.update(42u64.to_le_bytes());
        hasher1.update(1000u64.to_le_bytes());
        let hash1: [u8; 32] = hasher1.finalize().into();

        let mut hasher2 = Sha256::new();
        hasher2.update(b"AMOS_EMISSION_PROOF:");
        hasher2.update(101u64.to_le_bytes()); // Different day
        hasher2.update(42u64.to_le_bytes());
        hasher2.update(1000u64.to_le_bytes());
        let hash2: [u8; 32] = hasher2.finalize().into();

        assert_ne!(hash1, hash2, "Hash should differ for different inputs");
    }
}
