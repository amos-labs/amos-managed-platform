# amos-platform

Multi-tenant control plane for AMOS. Manages harness lifecycle, billing, governance, and the sync API.

## Binary

```bash
AMOS__DATABASE__URL=postgres://user@localhost:5432/amos_platform_dev \
  AMOS__SERVER__PORT=4000 \
  cargo run --bin amos-platform
# → HTTP: http://localhost:4000, gRPC: localhost:4001
```

## Architecture

```
src/
├── routes/             # REST API handlers
│   ├── health.rs       # Health + readiness checks
│   ├── provisioning.rs # Harness lifecycle (create, start, stop, deprovision)
│   ├── sync.rs         # Harness sync (heartbeat, config, activity, version)
│   ├── governance.rs   # Proposals, voting, delegation
│   ├── billing.rs      # Subscription plans, usage tracking
│   └── token.rs        # Token economics endpoints
├── provisioning/       # Docker-based harness provisioning (bollard)
├── solana/             # Solana RPC client (treasury, governance, bounty programs)
├── billing/            # Subscription plans, compute cost tracking
├── services/           # Business logic services
│   ├── bounty.rs       # Bounty generation + quality scoring
│   └── governance.rs   # On-chain governance operations
├── server.rs           # Axum server setup
├── state.rs            # Shared application state
└── main.rs             # Entry point
```

## Key Concepts

**Provisioning**: Creates Docker containers for per-customer harness instances via the bollard Docker API. Each harness gets its own postgres database.

**Sync API**: Harness instances periodically heartbeat the platform, pull config updates, and push usage metrics. Supports both managed and self-hosted deployments.

**Billing**: Tracks compute costs for managed deployments. AMOS token holders get discounts via relay staking. Token economy monetization is handled by the relay (3% protocol fee on bounty payouts), not the platform.

**Governance**: On-chain proposal creation, voting, and delegation via Solana programs.

**Bounty Service**: Nightly emission distribution -- calculates daily token rewards proportional to contributor points. Optionally submits on-chain proofs to Solana.
