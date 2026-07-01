# Platform Build Plan — the primitives under the CRM

**Status:** draft for review (2026-07-01). Companion: `CONSTRUCTION-CRM-PLAN.md` (the CRM built on these).
**Goal:** build the platform primitives that make **runtime-defined, conversationally-extensible, resellable vertical apps** possible. The construction CRM is the first consumer; every primitive here is built to be reused by the next vertical.

---

## Architecture baseline (what exists today)

- **Platform** (`amos-managed-platform`) = control plane: tenants, provisioning, billing/Stripe, RBAC, MCP verbs, proof/governance, OAuth, email. **No runtime data or UI lives here.**
- **Harness** (`amos-harness`) = the **per-tenant runtime**: schema (runtime JSONB collections/records), canvas, tools, agent loop, MCP, memory. The platform provisions **one primary harness per tenant** (`harness_instances`).
- **Deployed apps** (Cuspr, Nuvola) = bespoke containers via the `deploy` verb → ECS. Not runtime-extensible.
- **Engines** = governed MCP services fronted by the platform (finance stub exists in `src/mcp/finance.rs`).

## Core architecture decision (locked)

**Verticals like the CRM are HARNESS-NATIVE, not bespoke containers.** They are built from **runtime data** — schema collections + canvas views + automations — configured by a **template**, running on the harness runtime. This is the *only* model that supports conversational self-modification (add a collection/view by talking to Claude, no recompile, no redeploy). Cuspr/Nuvola-style container apps stay supported, but the CRM is not one of them.

The **component / template / engine** vocabulary replaces **packages** (the dead compiled-plugin system — see P8).

---

## Phase 0 — Decisions to lock before building

| # | Decision | Recommendation |
|---|---|---|
| 0.1 | **Tenancy & economics** — harness-per-builder (isolation, reuse existing provisioning, higher $ floor) vs. shared multi-tenant CRM runtime (density, cheaper, more isolation work). | Start **harness-per-tenant** (reuse `provision_harness`); revisit a shared runtime only if per-builder cost breaks the price point. Flag as the one to watch. |
| 0.2 | **Permission-tier model** — how deep each of AMOS-core / reseller / builder can modify. | Three tiers, path/collection-scoped, enforced by the fence (P4). Design in P4. |
| 0.3 | **Overlay/upgrade model** — how tenant customizations coexist with upstream template upgrades. | **Additive overlays** (customizations layer on top, never edit template core) + versioned templates + Claude-mediated merge for the rare conflict (P5). |
| 0.4 | **Retire packages** — remove the compiled-plugin fossil. | Yes — after mining `amos-family-finance` for the finance engine + library (P8). |

---

## Primitives to build

### P1 — Component + Template system  *(foundational — everything depends on this)*
The runtime-data successor to packages.
- **Component definition** (data, not compiled): a named, versioned bundle of `{ collection schemas, canvas views, automations, required engines }`.
- **Template**: a composition of components + enabled engines + seed data. The unit a tenant is provisioned from.
- **Registry/catalog** of components + templates (DB-backed).
- **`apply_template(tenant, template@version)`**: seeds a tenant's harness runtime (creates collections, installs canvas views, registers automations, enables engines).
- **Tenant overlay store**: per-tenant additions/changes recorded as a layer distinct from the template (basis for upgrades in P5).
- **MCP verbs:** `list_templates`, `list_components`, `apply_template`, `template_status`.
- **Acceptance:** apply a trivial 2-collection template to a fresh tenant → collections + a canvas view exist and are usable end-to-end.

### P2 — One-call tenant provisioning from a template  *(extends the activation kit)*
- A governed, proof-carrying verb: create tenant → provision primary harness → `apply_template` → seed → return "ready" with the login/connect details.
- Reuses existing `provision_harness`; adds the template-apply step.
- The **create-user admin endpoint** (already shipped) + this = a rep can stand up a builder in minutes.
- **Acceptance:** one call produces a working, seeded tenant a user can log into and a Claude can operate.

### P3 — Reseller / partner model + Stripe Connect  *(the channel — v1)*
- **Partner entity** + **partner → child-tenant** link + **attribution** (which partner sold which tenant).
- **Stripe Connect**: partner onboarding (Express), rev-share split on each child tenant's subscription, payout + reporting.
- **Billing model — "smart infra" + licensed software** (two axes):
  - **Smart infra** — the tiered base ($99–$899: compute + the MCP operator + *bundled* engines like finance & marketing). ~40% margin; the differentiated substrate, not commodity hosting. **Every tenant pays this.**
  - **Licensed software** — net-new AMOS products (the CRM, designated premium engines) as separate subscription line items on top. SaaS margin — where value is captured. CRM license = **flat fee + seat bands** (≤10, ≤50, …).
  - A per-item **`designation` flag** decides bundled-into-infra vs. licensed (bundle finance/marketing + all MCP access now; the CRM is licensed). Preserves the lever to meter later.
  - **Bring-your-own apps (Cuspr/Nuvola, any dev) pay smart infra only** — no software license; they run their own business model. AMOS as neutral smart infra.
  - **Rev-share applies to the license margin, not infra.** Entitlement checks gate licensed products/engines.
- **Acceptance:** a partner is onboarded to Connect; a child tenant's license line splits to the partner automatically (infra is not shared); attribution is queryable.

### P4 — Permission tiers / the fence  *(governance — makes self-mod sellable)*
Generalize RBAC + proofgate from a global flag to **per-actor/path policy** (this is proofgate#11 / the "dentist scenario," productized).
- Three tiers: **AMOS-core** (protected — billing, isolation, platform internals, other tenants), **reseller** (may shape the template they sell), **builder** (may customize their own instance: their views/fields/workflows).
- Enforced on every self-modification (which collections/views/automations/engines an actor may change).
- Owner-approval escalation for out-of-scope requests; **every change proof-carrying** (receipt).
- **Acceptance:** a builder-scoped actor can add a field/view but is structurally blocked (with a clear message) from touching billing, isolation, or another tenant.

### P5 — Upgrade / overlay engine  *(pull model, Claude-mediated)*
- **Template versioning**; tenant customizations stored as **additive overlays** (from P1).
- `check_upgrades(tenant)` → what's new upstream; `pull_upgrade(tenant, version)` → apply upstream changes, reconciling against the overlay.
- **Conflict detection** + a structured diff Claude can explain and resolve *with* the user.
- **Acceptance:** improve a template upstream → a customized tenant sees the available upgrade, pulls it, keeps its customizations, and Claude surfaces any real conflict in plain language.

### P6 — Canvas → informational console  *(repurpose, don't retire)*
Canvas shifts from "the app UI, generated" to **mission control for the environment**.
- Keep/build: dashboards, proof-receipt feed, "what your AI did / what changed," env + service status, upgrade-available notices.
- Plus the **high-consequence governance controls** as explicit UI: permissions (the fence), approve/reject changes + upgrades, plan/billing.
- Descope: arbitrary interactive-app-in-an-iframe generation (that was for the old "build/research in canvas" purpose — now conversational). Canvas owns *observe + run/manage the env*, not *create*.
- **Acceptance:** the console shows live env state + receipts and exposes permission/approval/billing controls; no authoring happens here.

### P7 — Engine framework  *(finance is the reference)*
- A clear contract for an engine: registers, is **enabled per tenant** (entitlement-gated), **MCP-fronted**, **scoped**, **proof-carrying on writes** (the pattern in `src/mcp/finance.rs`).
- Harden the finance engine from stub → real (job-costing is the CRM's first use).
- **Acceptance:** a tenant enables the finance engine; its verbs appear scoped + proof-carrying; a second engine could be added by following the contract.

### P8 — Retire packages  *(cleanup)*
- Mine `amos-packages/amos-family-finance` for finance domain logic/schema → into P7 engine + the component library.
- Remove `amos-harness/src/packages.rs`, the `amos-packages/*` crates, `pkg-*` Cargo features, and the `AmosPackage` trait if unused.
- **Acceptance:** harness builds clean with packages gone; nothing references the old plugin system.

---

## Build order & dependencies

```
P0 decisions
   │
   ├─ P1 Component/Template system ──┬─ P2 Provisioning-from-template
   │                                 ├─ P5 Upgrade/overlay engine
   │                                 └─ P7 Engine framework
   ├─ P3 Reseller + Stripe Connect
   ├─ P4 Permission tiers / fence  (pairs with P1 + P5)
   ├─ P6 Canvas → console          (parallel, mostly independent)
   └─ P8 Retire packages           (cleanup, anytime)
```
**Critical path for the CRM:** P1 → P2 → (P7 for job-costing) → then CRM Phase 1. P3 + P4 unblock the reseller motion. P5 + P4 unblock self-serve upgrades/extension.

## Open questions
- 0.1 tenancy/economics (harness-per-tenant vs shared) — decide with real per-builder cost numbers.
- Exact overlay representation (P5) — how a customization is stored so it survives upstream merges.
- Whether components can carry compiled tools or must be automations-only (P1/P7 boundary).
