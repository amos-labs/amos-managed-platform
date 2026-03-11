//! On-chain governance system for AMOS platform.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Proposal for platform changes, features, or parameter updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub id: Uuid,
    pub title: String,
    pub description: String,
    pub proposer_wallet: String,
    pub status: ProposalStatus,
    pub proposal_type: ProposalType,
    pub votes_for: u64,
    pub votes_against: u64,
    pub total_voting_power: u64,
    pub created_at: DateTime<Utc>,
    pub voting_starts_at: Option<DateTime<Utc>>,
    pub voting_deadline: Option<DateTime<Utc>>,
    pub executed_at: Option<DateTime<Utc>>,
    /// Link to GitHub PR or discussion.
    pub repository_url: Option<String>,
    /// Milestone breakdown for research proposals.
    pub milestone_plan: Option<String>,
}

/// Proposal lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    /// Drafted but not yet submitted on-chain.
    Draft,
    /// Submitted on-chain, voting not yet started.
    Submitted,
    /// Active voting period.
    Active,
    /// Development in progress (feature proposals).
    InDevelopment,
    /// Code review phase.
    InReview,
    /// A/B testing phase.
    InAbTest,
    /// Awaiting customer feedback.
    AwaitingFeedback,
    /// Awaiting steward approval.
    AwaitingStewardApproval,
    /// Passed all gates, approved for merge.
    Approved,
    /// Merged to production.
    Merged,
    /// Voting failed or rejected by stewards.
    Rejected,
    /// Canceled by proposer.
    Cancelled,
}

/// Type of proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalType {
    /// New feature or enhancement.
    Feature,
    /// Parameter change (e.g., decay rate, emission schedule).
    Parameter,
    /// Treasury spending proposal.
    Treasury,
    /// Research proposal (staged payouts: 40/30/30).
    Research,
}

/// Quality gate type from the whitepaper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityGate {
    /// Performance benchmarks must show improvement.
    Benchmark,
    /// A/B test must show positive metrics.
    AbTest,
    /// Customer feedback must meet threshold (10+ responses, >70% positive).
    CustomerFeedback,
    /// Steward approval required for merge.
    StewardApproval,
}

impl QualityGate {
    /// Human-readable description of this gate.
    pub fn description(&self) -> &'static str {
        match self {
            Self::Benchmark => "Performance benchmarks must show improvement over baseline",
            Self::AbTest => "A/B test with >50% of users must show positive metrics",
            Self::CustomerFeedback => "At least 10 customer responses with >70% satisfaction",
            Self::StewardApproval => "Manual approval from platform steward required",
        }
    }
}

/// Vote record on a proposal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vote {
    pub id: Uuid,
    pub proposal_id: Uuid,
    pub voter_wallet: String,
    pub weight: u64, // Voting power (stake amount)
    pub support: bool, // true = for, false = against
    pub timestamp: DateTime<Utc>,
    /// If this vote was delegated.
    pub delegate_from: Option<String>,
}

/// Result of a quality gate check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateCheckResult {
    pub gate_type: QualityGate,
    pub passed: bool,
    pub details: String,
    pub checked_at: DateTime<Utc>,
    pub checked_by: Option<String>,
}

impl Proposal {
    /// Check if proposal can transition to the next status.
    pub fn can_transition_to(&self, new_status: ProposalStatus) -> Result<(), String> {
        use ProposalStatus::*;

        let allowed = match (self.status, new_status) {
            (Draft, Submitted) => true,
            (Submitted, Active) => true,
            (Active, InDevelopment) | (Active, Rejected) => true,
            (InDevelopment, InReview) => true,
            (InReview, InAbTest) => true,
            (InAbTest, AwaitingFeedback) => true,
            (AwaitingFeedback, AwaitingStewardApproval) => true,
            (AwaitingStewardApproval, Approved) | (AwaitingStewardApproval, Rejected) => true,
            (Approved, Merged) => true,
            (_, Cancelled) => true, // Can always cancel
            _ => false,
        };

        if allowed {
            Ok(())
        } else {
            Err(format!(
                "Cannot transition from {:?} to {:?}",
                self.status, new_status
            ))
        }
    }

    /// Check if voting is currently open.
    pub fn is_voting_open(&self) -> bool {
        self.status == ProposalStatus::Active
            && self
                .voting_deadline
                .map(|deadline| Utc::now() < deadline)
                .unwrap_or(false)
    }

    /// Calculate voting percentage for.
    pub fn percentage_for(&self) -> f64 {
        if self.total_voting_power == 0 {
            return 0.0;
        }
        (self.votes_for as f64 / self.total_voting_power as f64) * 100.0
    }

    /// Check if proposal has passed quorum and majority.
    pub fn has_passed(&self) -> bool {
        // TODO: Load quorum/threshold from on-chain config
        const QUORUM_BPS: u64 = 1_000; // 10% of total stake must vote
        const THRESHOLD_BPS: u64 = 5_000; // 50% of votes must be for

        let total_votes = self.votes_for + self.votes_against;
        if total_votes == 0 {
            return false;
        }

        // Check quorum (placeholder: assume 1M total stake)
        let quorum_met = total_votes >= 100_000; // 10% of 1M

        // Check majority
        let majority = (self.votes_for * 10_000) / total_votes >= THRESHOLD_BPS;

        quorum_met && majority
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposal_status_transitions() {
        let mut proposal = Proposal {
            id: Uuid::new_v4(),
            title: "Test Proposal".into(),
            description: "Test".into(),
            proposer_wallet: "wallet123".into(),
            status: ProposalStatus::Draft,
            proposal_type: ProposalType::Feature,
            votes_for: 0,
            votes_against: 0,
            total_voting_power: 0,
            created_at: Utc::now(),
            voting_starts_at: None,
            voting_deadline: None,
            executed_at: None,
            repository_url: None,
            milestone_plan: None,
        };

        assert!(proposal.can_transition_to(ProposalStatus::Submitted).is_ok());
        assert!(proposal.can_transition_to(ProposalStatus::Merged).is_err());
    }

    #[test]
    fn voting_percentage_calculation() {
        let proposal = Proposal {
            id: Uuid::new_v4(),
            title: "Test".into(),
            description: "Test".into(),
            proposer_wallet: "wallet".into(),
            status: ProposalStatus::Active,
            proposal_type: ProposalType::Feature,
            votes_for: 75_000,
            votes_against: 25_000,
            total_voting_power: 100_000,
            created_at: Utc::now(),
            voting_starts_at: None,
            voting_deadline: None,
            executed_at: None,
            repository_url: None,
            milestone_plan: None,
        };

        assert_eq!(proposal.percentage_for(), 75.0);
        assert!(proposal.has_passed());
    }
}
