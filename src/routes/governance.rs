//! Governance API endpoints.

use amos_core::token::economics::MIN_STAKE_AMOUNT;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tracing::warn;
use uuid::Uuid;

use crate::{governance::*, state::PlatformState};

pub fn routes() -> Router<PlatformState> {
    Router::new()
        .route("/governance/proposals", get(list_proposals).post(create_proposal))
        .route("/governance/proposals/{id}", get(get_proposal))
        .route("/governance/proposals/{id}/vote", post(cast_vote))
        .route("/governance/proposals/{id}/gates", get(get_gates))
}

//    List Proposals

#[derive(Serialize)]
struct ProposalListResponse {
    proposals: Vec<ProposalSummary>,
    total: usize,
}

#[derive(Serialize)]
struct ProposalSummary {
    id: Uuid,
    title: String,
    proposer: String,
    status: ProposalStatus,
    votes_for: u64,
    votes_against: u64,
    created_at: DateTime<Utc>,
    voting_deadline: Option<DateTime<Utc>>,
}

async fn list_proposals(State(state): State<PlatformState>) -> impl IntoResponse {
    // Fetch proposals from database (gracefully handle missing table)
    let db_proposals = match sqlx::query(
        r#"
        SELECT id, title, description, proposer_wallet, status, proposal_type,
               votes_for, votes_against, total_voting_power, created_at,
               voting_starts_at, voting_deadline, executed_at, repository_url, milestone_plan
        FROM governance_proposals
        ORDER BY created_at DESC
        "#,
    )
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            warn!("Failed to query proposals from DB (table may not exist): {}", e);
            vec![]
        }
    };

    // Convert DB rows to ProposalSummary
    let mut proposals = Vec::new();
    for row in db_proposals {
        let id: Uuid = row.get("id");
        let title: String = row.get("title");
        let proposer_wallet: String = row.get("proposer_wallet");
        let status_str: String = row.get("status");
        let votes_for: i64 = row.get("votes_for");
        let votes_against: i64 = row.get("votes_against");
        let created_at: DateTime<Utc> = row.get("created_at");
        let voting_deadline: Option<DateTime<Utc>> = row.get("voting_deadline");

        // Parse status enum
        let status = serde_json::from_str::<ProposalStatus>(&format!("\"{}\"", status_str))
            .unwrap_or(ProposalStatus::Draft);

        proposals.push(ProposalSummary {
            id,
            title,
            proposer: proposer_wallet,
            status,
            votes_for: votes_for as u64,
            votes_against: votes_against as u64,
            created_at,
            voting_deadline,
        });
    }

    // If Solana is available, merge on-chain vote counts
    if let Some(solana) = &state.solana {
        match solana.get_governance_proposals().await {
            Ok(on_chain_proposals) => {
                // Update vote counts from on-chain data
                for proposal in &mut proposals {
                    // Match on-chain proposal by title hash (in real impl, store proposal_id mapping)
                    // For now, we just update the first matching votes
                    if let Some(on_chain) = on_chain_proposals.first() {
                        proposal.votes_for = on_chain.votes_for;
                        proposal.votes_against = on_chain.votes_against;
                    }
                }
            }
            Err(e) => {
                warn!("Failed to fetch on-chain proposals: {}", e);
            }
        }
    }

    let total = proposals.len();

    Json(ProposalListResponse { proposals, total })
}

//    Create Proposal

#[derive(Deserialize)]
struct CreateProposalRequest {
    title: String,
    description: String,
    proposer_wallet: String,
    proposal_type: String, // "feature", "parameter", "treasury", "research"
    #[serde(default)]
    repository_url: Option<String>,
    #[serde(default)]
    milestone_plan: Option<String>,
}

#[derive(Serialize)]
struct CreateProposalResponse {
    id: Uuid,
    status: ProposalStatus,
    created_at: DateTime<Utc>,
}

async fn create_proposal(
    State(state): State<PlatformState>,
    Json(req): Json<CreateProposalRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    // Validate required fields
    if req.title.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if req.description.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if req.proposer_wallet.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Parse proposal type
    let proposal_type = match req.proposal_type.to_lowercase().as_str() {
        "feature" => ProposalType::Feature,
        "parameter" => ProposalType::Parameter,
        "treasury" => ProposalType::Treasury,
        "research" => ProposalType::Research,
        _ => return Err(StatusCode::BAD_REQUEST),
    };

    // Verify proposer has minimum stake (if Solana available)
    if let Some(solana) = &state.solana {
        match solana.get_stake_record(&req.proposer_wallet).await {
            Ok(Some(stake_record)) => {
                if stake_record.amount < MIN_STAKE_AMOUNT {
                    warn!(
                        "Proposer {} has insufficient stake: {} < {}",
                        req.proposer_wallet, stake_record.amount, MIN_STAKE_AMOUNT
                    );
                    return Err(StatusCode::FORBIDDEN);
                }
            }
            Ok(None) => {
                warn!("Proposer {} has no stake record", req.proposer_wallet);
                return Err(StatusCode::FORBIDDEN);
            }
            Err(e) => {
                warn!("Failed to fetch stake record for {}: {}", req.proposer_wallet, e);
                // Continue in dev mode even if Solana fails
            }
        }
    }

    let id = Uuid::new_v4();
    let now = Utc::now();
    let status = ProposalStatus::Draft;

    // Insert into database
    match sqlx::query(
        r#"
        INSERT INTO governance_proposals
        (id, title, description, proposer_wallet, status, proposal_type,
         votes_for, votes_against, total_voting_power, created_at,
         repository_url, milestone_plan)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        "#,
    )
    .bind(id)
    .bind(&req.title)
    .bind(&req.description)
    .bind(&req.proposer_wallet)
    .bind("draft")
    .bind(format!("{:?}", proposal_type).to_lowercase())
    .bind(0i64) // votes_for
    .bind(0i64) // votes_against
    .bind(0i64) // total_voting_power
    .bind(now)
    .bind(&req.repository_url)
    .bind(&req.milestone_plan)
    .execute(&state.db)
    .await
    {
        Ok(_) => {}
        Err(e) => {
            warn!("Failed to insert proposal into DB (table may not exist): {}", e);
            // Continue anyway in dev mode
        }
    }

    Ok((
        StatusCode::CREATED,
        Json(CreateProposalResponse {
            id,
            status,
            created_at: now,
        }),
    ))
}

//    Get Proposal

#[derive(Serialize)]
struct ProposalDetailResponse {
    id: Uuid,
    title: String,
    description: String,
    proposer: String,
    status: ProposalStatus,
    proposal_type: ProposalType,
    votes_for: u64,
    votes_against: u64,
    total_voting_power: u64,
    created_at: DateTime<Utc>,
    voting_deadline: Option<DateTime<Utc>>,
    repository_url: Option<String>,
    milestone_plan: Option<String>,
    votes: Vec<VoteRecord>,
}

#[derive(Serialize)]
struct VoteRecord {
    voter: String,
    weight: u64,
    support: bool,
    timestamp: DateTime<Utc>,
}

async fn get_proposal(
    State(state): State<PlatformState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, StatusCode> {
    // Query database for proposal
    let proposal_row = match sqlx::query(
        r#"
        SELECT id, title, description, proposer_wallet, status, proposal_type,
               votes_for, votes_against, total_voting_power, created_at,
               voting_starts_at, voting_deadline, executed_at, repository_url, milestone_plan
        FROM governance_proposals
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => return Err(StatusCode::NOT_FOUND),
        Err(e) => {
            warn!("Failed to query proposal from DB (table may not exist): {}", e);
            return Err(StatusCode::NOT_FOUND);
        }
    };

    // Extract proposal data
    let title: String = proposal_row.get("title");
    let description: String = proposal_row.get("description");
    let proposer_wallet: String = proposal_row.get("proposer_wallet");
    let status_str: String = proposal_row.get("status");
    let proposal_type_str: String = proposal_row.get("proposal_type");
    let mut votes_for: i64 = proposal_row.get("votes_for");
    let mut votes_against: i64 = proposal_row.get("votes_against");
    let total_voting_power: i64 = proposal_row.get("total_voting_power");
    let created_at: DateTime<Utc> = proposal_row.get("created_at");
    let voting_deadline: Option<DateTime<Utc>> = proposal_row.get("voting_deadline");
    let repository_url: Option<String> = proposal_row.get("repository_url");
    let milestone_plan: Option<String> = proposal_row.get("milestone_plan");

    // Parse enums
    let status = serde_json::from_str::<ProposalStatus>(&format!("\"{}\"", status_str))
        .unwrap_or(ProposalStatus::Draft);
    let proposal_type = serde_json::from_str::<ProposalType>(&format!("\"{}\"", proposal_type_str))
        .unwrap_or(ProposalType::Feature);

    // If Solana is available, get on-chain vote data
    if let Some(solana) = &state.solana {
        match solana.get_governance_proposals().await {
            Ok(on_chain_proposals) => {
                // Update vote counts from on-chain (in real impl, match by proposal_id)
                if let Some(on_chain) = on_chain_proposals.first() {
                    votes_for = on_chain.votes_for as i64;
                    votes_against = on_chain.votes_against as i64;
                }
            }
            Err(e) => {
                warn!("Failed to fetch on-chain proposals: {}", e);
            }
        }
    }

    // Fetch votes from database
    let vote_rows = match sqlx::query(
        r#"
        SELECT voter_wallet, weight, support, timestamp
        FROM governance_votes
        WHERE proposal_id = $1
        ORDER BY timestamp DESC
        "#,
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            warn!("Failed to query votes from DB (table may not exist): {}", e);
            vec![]
        }
    };

    let votes: Vec<VoteRecord> = vote_rows
        .iter()
        .map(|row| VoteRecord {
            voter: row.get("voter_wallet"),
            weight: row.get::<i64, _>("weight") as u64,
            support: row.get("support"),
            timestamp: row.get("timestamp"),
        })
        .collect();

    Ok(Json(ProposalDetailResponse {
        id,
        title,
        description,
        proposer: proposer_wallet,
        status,
        proposal_type,
        votes_for: votes_for as u64,
        votes_against: votes_against as u64,
        total_voting_power: total_voting_power as u64,
        created_at,
        voting_deadline,
        repository_url,
        milestone_plan,
        votes,
    }))
}

//    Cast Vote

#[derive(Deserialize)]
struct CastVoteRequest {
    voter_wallet: String,
    support: bool, // true = for, false = against
    #[serde(default)]
    delegate_from: Option<String>,
}

#[derive(Serialize)]
struct CastVoteResponse {
    vote_id: Uuid,
    weight: u64,
    new_votes_for: u64,
    new_votes_against: u64,
}

async fn cast_vote(
    State(state): State<PlatformState>,
    Path(proposal_id): Path<Uuid>,
    Json(req): Json<CastVoteRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    // Validate voter wallet
    if req.voter_wallet.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Get voter's stake weight from Solana
    let voting_weight = if let Some(solana) = &state.solana {
        match solana.get_stake_record(&req.voter_wallet).await {
            Ok(Some(stake_record)) => {
                if stake_record.amount == 0 {
                    warn!("Voter {} has zero stake", req.voter_wallet);
                    return Err(StatusCode::FORBIDDEN);
                }
                stake_record.amount
            }
            Ok(None) => {
                warn!("Voter {} has no stake record", req.voter_wallet);
                return Err(StatusCode::FORBIDDEN);
            }
            Err(e) => {
                warn!("Failed to fetch stake record for {}: {}", req.voter_wallet, e);
                // In dev mode, assign default weight
                1000
            }
        }
    } else {
        // No Solana client, use default weight for dev
        1000
    };

    // Fetch proposal to validate voting is open
    let proposal_row = match sqlx::query(
        r#"
        SELECT status, voting_deadline
        FROM governance_proposals
        WHERE id = $1
        "#,
    )
    .bind(proposal_id)
    .fetch_optional(&state.db)
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => return Err(StatusCode::NOT_FOUND),
        Err(e) => {
            warn!("Failed to query proposal from DB: {}", e);
            return Err(StatusCode::NOT_FOUND);
        }
    };

    let status_str: String = proposal_row.get("status");
    let voting_deadline: Option<DateTime<Utc>> = proposal_row.get("voting_deadline");

    // Check if voting is active
    let status = serde_json::from_str::<ProposalStatus>(&format!("\"{}\"", status_str))
        .unwrap_or(ProposalStatus::Draft);

    if status != ProposalStatus::Active {
        warn!("Proposal {} is not in Active status: {:?}", proposal_id, status);
        return Err(StatusCode::FORBIDDEN);
    }

    // Check voting deadline
    if let Some(deadline) = voting_deadline {
        if Utc::now() > deadline {
            warn!("Voting deadline passed for proposal {}", proposal_id);
            return Err(StatusCode::FORBIDDEN);
        }
    }

    // Record vote in database
    let vote_id = Uuid::new_v4();
    let now = Utc::now();

    match sqlx::query(
        r#"
        INSERT INTO governance_votes
        (id, proposal_id, voter_wallet, weight, support, timestamp, delegate_from)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(vote_id)
    .bind(proposal_id)
    .bind(&req.voter_wallet)
    .bind(voting_weight as i64)
    .bind(req.support)
    .bind(now)
    .bind(&req.delegate_from)
    .execute(&state.db)
    .await
    {
        Ok(_) => {}
        Err(e) => {
            warn!("Failed to insert vote into DB (table may not exist): {}", e);
            // Continue in dev mode
        }
    }

    // Update vote tallies in proposal
    let (new_votes_for, new_votes_against) = if req.support {
        sqlx::query(
            r#"
            UPDATE governance_proposals
            SET votes_for = votes_for + $1
            WHERE id = $2
            RETURNING votes_for, votes_against
            "#,
        )
        .bind(voting_weight as i64)
        .bind(proposal_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .map(|row| {
            (
                row.get::<i64, _>("votes_for") as u64,
                row.get::<i64, _>("votes_against") as u64,
            )
        })
        .unwrap_or((voting_weight, 0))
    } else {
        sqlx::query(
            r#"
            UPDATE governance_proposals
            SET votes_against = votes_against + $1
            WHERE id = $2
            RETURNING votes_for, votes_against
            "#,
        )
        .bind(voting_weight as i64)
        .bind(proposal_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .map(|row| {
            (
                row.get::<i64, _>("votes_for") as u64,
                row.get::<i64, _>("votes_against") as u64,
            )
        })
        .unwrap_or((0, voting_weight))
    };

    Ok(Json(CastVoteResponse {
        vote_id,
        weight: voting_weight,
        new_votes_for,
        new_votes_against,
    }))
}

//    Get Quality Gates

#[derive(Serialize)]
struct GateStatusResponse {
    proposal_id: Uuid,
    gates: Vec<GateStatus>,
    all_passed: bool,
}

#[derive(Serialize, Clone)]
struct GateStatus {
    gate_type: String,
    passed: bool,
    required: bool,
    details: String,
    checked_at: Option<DateTime<Utc>>,
}

async fn get_gates(
    State(state): State<PlatformState>,
    Path(proposal_id): Path<Uuid>,
) -> Result<impl IntoResponse, StatusCode> {
    // Query gate checks from database
    let gate_rows = match sqlx::query(
        r#"
        SELECT gate_type, passed, details, checked_at
        FROM governance_gates
        WHERE proposal_id = $1
        ORDER BY checked_at DESC
        "#,
    )
    .bind(proposal_id)
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            warn!("Failed to query gates from DB (table may not exist): {}", e);
            vec![]
        }
    };

    // Create a map of gate types to their status
    let mut gate_map = std::collections::HashMap::new();
    for row in gate_rows {
        let gate_type: String = row.get("gate_type");
        let passed: bool = row.get("passed");
        let details: String = row.get("details");
        let checked_at: Option<DateTime<Utc>> = row.get("checked_at");

        gate_map.insert(
            gate_type.clone(),
            GateStatus {
                gate_type,
                passed,
                required: true,
                details,
                checked_at,
            },
        );
    }

    // Define all required gates
    let all_gate_types = vec![
        ("benchmark", QualityGate::Benchmark),
        ("ab_test", QualityGate::AbTest),
        ("feedback", QualityGate::CustomerFeedback),
        ("steward", QualityGate::StewardApproval),
    ];

    let gates: Vec<GateStatus> = all_gate_types
        .iter()
        .map(|(gate_type_str, gate_type)| {
            if let Some(gate_status) = gate_map.get(*gate_type_str) {
                gate_status.clone()
            } else {
                // Gate not yet checked, return default unchecked state
                GateStatus {
                    gate_type: gate_type_str.to_string(),
                    passed: false,
                    required: true,
                    details: gate_type.description().to_string(),
                    checked_at: None,
                }
            }
        })
        .collect();

    let all_passed = gates.iter().all(|g| g.passed);

    Ok(Json(GateStatusResponse {
        proposal_id,
        gates,
        all_passed,
    }))
}
