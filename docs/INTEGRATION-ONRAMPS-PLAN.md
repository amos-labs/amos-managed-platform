# Integration & On-ramps Plan — how data + apps enter the ecosystem

**Status:** draft for review (2026-07-01). Third of three: `PLATFORM-BUILD-PLAN.md`, `CONSTRUCTION-CRM-PLAN.md`, this.
**Goal:** define the primitives that bring **any** data, app, or SaaS into the AMOS ecosystem so the **company brain** (Claude over MCP) can operate all of it uniformly. This is the "get everything in one place" layer — where "one place" means **one addressable, governed surface**, not necessarily one physical database.

---

## The principle
A business runs on a spread of tools. AMOS's job is to make them behave as **one brain**:
- **Replace the many** ancillary SaaS with net-new AMOS solutions (built from components).
- **Connect the few** anchor systems the owner will never leave (QBO for the books, etc.) — keep them as system-of-record, integrate them.
- **Port** existing apps that come over whole (Cuspr, Nuvola).

Unification happens at the **verb/query layer**: whatever the source, the brain sees one operable surface. Physically consolidate only where it earns its keep (net-new data lives in AMOS natively; anchors and ported apps keep their own stores but are exposed uniformly).

## What already exists (the raw materials — we're naming the pattern)
- `db_query` / `db_write` — the brain can already read/write a tenant app's own DB.
- **The finance engine already fronts QBO** (`qbo_accounts` + finance verbs) — the anchor-connector pattern, unnamed.
- **OAuth connector** (shipped) — how we authenticate to an external SaaS and to our own MCP.
- `whoami` / `get_started` — self-description; the seed of a catalog.

---

## On-ramps to build

### O1 — Connector  *(anchors you keep: QBO, etc.)*
- OAuth to the SaaS (reuse the OAuth work) + a mapping of its objects → AMOS-addressable **read/write verbs**, fronted **as an engine** (finance + QBO is the reference — generalize it).
- SaaS stays **system-of-record**; AMOS **mirrors the figures that matter** into a collection so the brain can cross-reference + reason (e.g. QBO actuals vs. CRM job-costing); writes flow back where allowed.
- Governed: scoped, proof-carrying, entitlement-gated.
- **Deliverable:** a connector framework + **QBO as the first real connector**.
- **Acceptance:** connect QBO via OAuth → the brain reads actuals, cross-references CRM job-costing, and a governed write flows back.

### O2 — Port-and-expose  *(existing apps: the Cuspr/Nuvola pattern, finished)*
- Host it (done via `deploy`) **+ make it brain-operable**: expose its data via the db verbs, its **key operations as MCP verbs**, and register it in the tenant catalog (O4).
- The gap today: Cuspr/Nuvola are *hosted* but not fully *surfaced as verbs* — a systematic "ported app → brain-operable" recipe closes it.
- **Deliverable:** a port-and-expose recipe/checklist + verb registration; apply to Nuvola/Cuspr as the reference.
- **Acceptance:** the brain operates a ported app's core actions via MCP and queries its data, governed.

### O3 — Ingest / migrate  *(replacing ancillary SaaS)*
- Pull data out of the old tool (export/API) → **map** → load into net-new AMOS components/collections. The CRM's Phase-4 import, generalized: **source adapters + field mapping, Claude-assisted**.
- **Deliverable:** a reusable ingest primitive + a Claude-assisted mapping flow; reference = the launch builder's CRM export.
- **Acceptance:** a builder's export lands cleanly + mapped into AMOS collections, brain-operable.

### O4 — Unified ecosystem catalog  *(the brain's map — small, high-leverage, build early)*
- A per-tenant catalog of **everything**: collections, connected anchors, ported apps, enabled engines, templates + versions.
- The brain reads it **on connect** (extend `whoami`/`get_started`) so Claude immediately knows the full surface it can operate.
- **Deliverable:** a catalog MCP verb aggregating all sources, wired into the connect flow.
- **Acceptance:** on connect, Claude accurately enumerates the tenant's entire operable surface.

---

## Relation to the platform plan
- Connectors (O1) are **a kind of engine** → build on the **P7 engine framework**.
- All on-ramps are governed by the **P4 permission fence** (what the brain / a given actor may read or write per source).
- The catalog (O4) extends the activation-kit self-description.

## Suggested sequence
1. **O4 catalog** — the brain needs its map first; small and high-leverage.
2. **O1 QBO connector** — the highest-value anchor; proves the connector-as-engine pattern.
3. **O3 ingest** — needed for the CRM launch (replacing the builder's ancillary SaaS).
4. **O2 port-and-expose recipe** — formalize what Cuspr/Nuvola did so the next port is a checklist.

## Open questions
- **Mirror vs. live-read** from anchors — data freshness vs. coupling; per-connector policy.
- **Write-back policy** — which anchors accept governed writes vs. read-only.
- **Catalog freshness** — how it stays current as the tenant changes.
