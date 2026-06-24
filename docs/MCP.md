# AMOS Platform MCP Server

The AMOS platform exposes a **Model Context Protocol (MCP)** server so an AI
client (Claude Code, the Claude desktop/web apps, or any MCP-capable agent) can
*operate* your governed AMOS environment with tool calls instead of clicking a
console or memorizing AWS. It is the primary interface to the platform — the UI
is a thin window onto the same operations.

Every consequential action emits a **proof-carrying receipt** (intent,
guardrails enforced, verification checks, outputs) that you can read back with
`list_receipts` — so what the AI does on your behalf is accountable and
auditable.

> This file is the **source of truth** for the MCP surface. It is kept current
> with the server, so tools added to the platform appear here.

## Connect

- **Endpoint:** `https://app.amoslabs.com/mcp`
- **Transport:** MCP Streamable HTTP (JSON-RPC 2.0 over `POST /mcp`)
- **Auth:** every request carries `Authorization: Bearer <token>` — a platform
  JWT (from login) or a tenant **API key**. Tools are scoped to your tenant; an
  admin may target another tenant by passing `tenant_id`.

### Add it to Claude Code

```bash
claude mcp add --transport http amos https://app.amoslabs.com/mcp \
    --header "Authorization: Bearer $AMOS_TOKEN"
```

### Or via an `.mcp.json` (checked into a repo so a whole team picks it up)

```json
{
  "mcpServers": {
    "amos": {
      "type": "http",
      "url": "https://app.amoslabs.com/mcp",
      "headers": { "Authorization": "Bearer ${AMOS_TOKEN}" }
    }
  }
}
```

Set `AMOS_TOKEN` in your environment to your platform token (see **Getting a
token** below). A copy of this `.mcp.json` lives in the root of the Cuspr
(smilewise) repo so Cuspr's team can connect immediately.

### Getting a token

- **JWT:** log in to the platform (`POST /api/v1/auth/login`) — the access
  token is a Bearer JWT scoped to your tenant.
- **API key:** ask a tenant admin to issue a long-lived API key (stored hashed,
  in `api_keys`). Prefer API keys for unattended/agent use; rotate as needed.

## Tools

All tools are tenant-scoped. Operations that change state (`deploy_app`,
`build_image`, `app_control`, `harness_control`, …) emit an `OperationReceipt`.

### Environments (the AMOS harness runtime)

| Tool | What it does |
|---|---|
| `list_harnesses` | List your AMOS environments (harness instances) with status/health. Call first to discover what exists. |
| `provision_harness` | Provision/start a governed environment's container. |
| `harness_status` | Live runtime status + health of an environment by `harness_id`. |
| `harness_logs` | Recent logs for an environment (Docker: last lines; ECS: CloudWatch). |
| `harness_control` | Lifecycle: `start` / `stop` / `restart` / `deprovision`. |
| `get_harness_config` / `set_harness_config` | Read/update env config (enabled flag + feature flags). |
| `list_releases` | Platform releases (harness/agent images registered by CI). |

### App lifecycle (multi-service apps, e.g. Cuspr)

| Tool | What it does |
|---|---|
| `deploy_app` | Deploy a multi-service app. `provider:"docker"` (local) or `provider:"aws"` (Fargate: one ECS task of co-located containers behind the ALB; skips managed postgres/redis, injects their connection strings from Secrets Manager). Provide `spec` (AppSpec) or `compose_yaml`. Optional `aws` target (`secret_arn`, `target_group_arn`, `public_service`, …) and `callback_url` (ping-back). Returns a `deployment_id`. |
| `list_apps` | List your app deployments with per-service status. |
| `app_status` | Live status of a deployment. For AWS it surfaces the ECS rollout (state, running/desired, per-container). |
| `app_logs` | Recent logs for one service. For AWS deploys this reads **CloudWatch** directly — no AWS console needed. |
| `app_control` | `start` / `stop` / `restart` / `deprovision` (optionally one service; `destroy_data` to drop volumes). |

### Build-as-a-service

| Tool | What it does |
|---|---|
| `build_image` | Build a container image in the cloud (CodeBuild) from a build context and push to ECR — **no local Docker**. Args: `context_dir`, `image_name`, `dockerfile`, `tag`, `build_args` (e.g. `{"BASE_IMAGE":"…"}`), optional `callback_url`. Returns a `build_id`; runs async. |
| `build_status` | Status/phase of a build; on success returns the pushed image **digest** + full `image_ref` (use it in a `deploy_app` spec). On failure, a CloudWatch logs link. |

### Proof / audit

| Tool | What it does |
|---|---|
| `list_receipts` | Proof-carrying operation receipts for your tenant (newest first): intent, guardrails enforced, verification checks + outcomes, outputs, and `verified`. The audit trail of what the AI did on your behalf. |

## Typical flow: build + deploy an app

```text
build_image(context_dir, image_name, build_args)  -> build_id
build_status(build_id, image_name)                -> image digest / image_ref
deploy_app(provider="aws", spec=<refs the digest>, aws={secret_arn, target_group_arn, …})
                                                  -> deployment_id
app_status(deployment_id)                          -> ECS rollout COMPLETED
app_logs(deployment_id, service="api")             -> CloudWatch logs
list_receipts(operation="deploy_app")              -> the proof it happened + verified
```

## Notes

- **Tenant scoping & RBAC:** every call is scoped to the caller's tenant.
  Fine-grained role/permission control (what the AI may do vs. what each user
  may do) is on the roadmap — see the platform README / control-plane docs.
- **Accountability:** prefer reading `list_receipts` over trusting tool output
  blindly; the receipt records what was verified.
