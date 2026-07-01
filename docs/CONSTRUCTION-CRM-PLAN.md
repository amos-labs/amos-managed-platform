# Construction CRM Build Plan — the product on top

**Status:** draft for review (2026-07-01). Depends on `PLATFORM-BUILD-PLAN.md`.
**Goal:** build the construction CRM as a **component library + template** on the platform primitives, onboard the launch builder (off their current SaaS), and light up the reseller channel. Every block ships as a **reusable library component**, not bespoke CRM code — the library is the asset that makes vertical #2 fast.

---

## What we're building (one sentence)
A construction CRM a builder buys to replace their SaaS — that their AI operates and they reshape by talking to it — sold AMOS-branded, direct and through a reseller channel.

## Architecture recap
Harness-native runtime. The CRM is **runtime data**: schema collections + canvas views + automations + enabled engines, composed as a **template**, applied to a builder's tenant. Operated by Claude over MCP; observed/managed in the AMOS console (canvas). See platform plan for the primitives (P1 template system, P2 provisioning, P3 reseller/Connect, P4 fence, P5 upgrades, P7 engines).

---

## Phase 1 — The component library (the BOM)
Build each as a library component = `{ collection schema, canvas views, automations, AI-operator behaviors }`. Generic first (reusable everywhere), then the construction vertical.

**Generic CRM components (reusable across every vertical):**
| Component | Collection(s) | Notes |
|---|---|---|
| `contacts` | people, companies | the relationship graph |
| `pipeline` | deals, stages | kanban; configurable stages |
| `activities` | tasks, events | calendar, reminders, follow-ups |
| `documents` | files (S3) | + e-sign hook |
| `comms-log` | messages | email/SMS history threaded to records → marketing engine later |
| `dashboards` | (views only) | canvas reports + saved views |

**Construction-vertical components:**
| Component | Collection(s) | Notes |
|---|---|---|
| `jobs` | jobs/projects | first-class, linked to deals + contacts |
| `bids` | estimates, bids | win/loss tracking |
| `change-orders` | change_orders | priced + approved against a job |
| `subs` | subcontractors | directory + assignment |
| `scheduling` | schedule | crew/job calendar |
| `job-costing` | (via finance engine) | cost vs. budget per job — **finance engine applied**, not a bespoke table |

- **Discipline:** build these as generic library parts even when it's slightly more work; construction-specific logic goes in the vertical components, not smeared into the generic ones.
- **Acceptance (Phase 1):** each component installs into a tenant via the template system, with working collections + at least one canvas view + its core automations.

## Phase 2 — The construction CRM template
- Compose the components above + enable the **finance engine** (for job-costing).
- Seed data (stages, example records for demo/trial).
- This is the **SKU a builder buys**. Versioned (feeds P5 upgrades).
- **Acceptance:** `apply_template(construction-crm)` on a fresh tenant yields a complete, usable CRM.

## Phase 3 — The AI operator layer
The "talk to your CRM" experience — CRM-specific MCP verbs + agent behaviors:
- Draft/send bid follow-ups; advance a deal; log/summarize an activity; chase an overdue invoice; weekly pipeline + job-status summary; "add a field/view for X" (self-mod, fenced by P4).
- **Acceptance:** a builder can run the core CRM workflow entirely by talking to Claude, and every mutation is proof-carrying.

## Phase 4 — Launch builder onboarding (the paying customer)
- Provision their tenant from the template (P2).
- **Migrate off their current SaaS:** import contacts, companies, jobs, pipeline (map their export → our collections).
- Tune the AI operator to their workflow; validate with their real data.
- **Go live** → they cancel their old SaaS sub. That's the proof point.
- **Acceptance:** the builder runs their business on the CRM; their SaaS subscription is cancelled.

## Phase 5 — Reseller enablement (the channel)
- Their sales team provisions a new builder in minutes (P2 one-call), AMOS-branded.
- Attribution + **Stripe Connect rev-share** live (P3); partner sees their book + payouts.
- Fence in place so a builder self-modifies safely (P4).
- **Acceptance:** a rep closes + stands up a new builder end-to-end without engineering; the partner is paid automatically.

## Phase 6 — Engines on top (the upsell / LTV)
- **Finance** beyond job-costing: invoicing, payments.
- **Marketing**: outreach/campaigns off the comms-log.
- Enable-able per tenant, entitlement-gated (P3/P7).
- **Acceptance:** a builder turns on an engine from the console and it's operating without re-platforming.

---

## Dependency map (CRM ↔ platform)
| CRM phase | Needs platform |
|---|---|
| 1 Component library | P1 (template/component system), P7 (engine framework, for job-costing) |
| 2 Template | P1 |
| 3 AI operator | P4 (fence, for self-mod) |
| 4 Launch onboarding | P2 (provisioning) |
| 5 Reseller | P3 (reseller/Connect), P4 (fence) |
| 6 Engines | P7, P3 (entitlements) |
| self-serve upgrades | P5 (overlay/upgrade), P4 |

## Suggested sequencing (both tracks interleaved)
1. **Platform P1 + CRM Phase 1 (generic components)** — prove the template/component system on real components.
2. **CRM Phase 1 (construction components) + P7 finance/job-costing + CRM Phase 2 template.**
3. **CRM Phase 3 AI operator + P2 provisioning + CRM Phase 4 launch builder** — get the first paying customer live.
4. **P3 reseller/Connect + P4 fence + CRM Phase 5** — turn on the channel.
5. **P5 upgrades/overlay + CRM Phase 6 engines** — self-serve extensibility + upsell.
6. **P8 retire packages** — cleanup, anytime after mining family-finance.

## Open questions (product)
- Which SaaS are they on now (drives the Phase-4 import mapping)?
- Do they want AMOS-branded confirmed (white-label stays deferred unless a resellee needs it)?
- v1 permission-tier depth for builders (what a builder can self-modify) — pairs with P4.
