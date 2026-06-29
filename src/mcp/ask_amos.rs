//! `ask_amos` — the natural-language manager front door (like Nuvola's
//! `ask_nuvola`). Runs an embedded Bedrock (Claude) loop whose tools are the
//! platform's READ-ONLY control verbs. The manager investigates the tenant's
//! environment and answers in plain language.
//!
//! v1 is **read/inspect only**: the exposed toolset excludes every mutating
//! verb, and the system prompt instructs the model to *propose* (not perform)
//! any change. Every inner verb call goes through `dispatch_tool`, so RBAC is
//! enforced against the caller's own scopes — the manager can never exceed the
//! key that invoked it.

use std::collections::HashMap;

use aws_sdk_bedrockruntime::types as brt;
use aws_smithy_types::{Document, Number};
use serde_json::{json, Value};

use super::{dispatch_tool, tool_definitions, ToolError};
use crate::auth::Claims;
use crate::state::PlatformState;

/// The only verbs the manager may call autonomously. Mutating verbs are
/// deliberately absent — the model proposes them in prose instead.
const READ_ONLY_TOOLS: &[&str] = &[
    "list_apps",
    "app_status",
    "app_logs",
    "deploy_status",
    "build_status",
    "db_query",
    "describe_table",
    "s3_list",
    "s3_get",
    "list_receipts",
    "list_harnesses",
    "harness_status",
    "whoami",
];

const MAX_ITERATIONS: u32 = 10;

fn model_id() -> String {
    // Mirror the harness default (amos-core config `default_model`); overridable.
    std::env::var("AMOS__BEDROCK__MODEL")
        .unwrap_or_else(|_| "us.anthropic.claude-sonnet-4-20250514-v1:0".to_string())
}

fn err_text(e: ToolError) -> String {
    match e {
        ToolError::NotFound => "not found".to_string(),
        ToolError::InvalidParams(s) | ToolError::Execution(s) | ToolError::Forbidden(s) => s,
    }
}

/// Run the manager loop for one prompt; returns `{answer, tools_used, iterations}`.
pub async fn run(state: &PlatformState, claims: &Claims, prompt: &str) -> Result<Value, ToolError> {
    let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;
    let client = aws_sdk_bedrockruntime::Client::new(&cfg);
    let model = model_id();

    let system = format!(
        "You are the AMOS manager for the '{}' environment. You operate and report on this \
         business's infrastructure and the apps and data running in it, on the user's behalf. \
         Use the tools to investigate and answer in plain language. You are READ-ONLY: never \
         assume a mutation happened. If the user wants something changed (deploy, scale, write \
         data, provision or tear down an environment, upload or delete files), describe exactly \
         what you would do — name the verb and its arguments — and say it must be run explicitly. \
         Do not claim you performed a change. Be concise and concrete.",
        claims.tenant_slug
    );

    // Build the Bedrock tool config from the advertised schemas, filtered to
    // the read-only allowlist.
    let defs = tool_definitions();
    let mut tools: Vec<brt::Tool> = Vec::new();
    for t in defs.as_array().into_iter().flatten() {
        let name = t.get("name").and_then(|v| v.as_str()).unwrap_or_default();
        if !READ_ONLY_TOOLS.contains(&name) {
            continue;
        }
        let desc = t
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let schema = t
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"}));
        let spec = brt::ToolSpecification::builder()
            .name(name)
            .description(desc)
            .input_schema(brt::ToolInputSchema::Json(json_to_document(&schema)))
            .build()
            .map_err(|e| ToolError::Execution(format!("tool spec: {e}")))?;
        tools.push(brt::Tool::ToolSpec(spec));
    }
    let tool_config = brt::ToolConfiguration::builder()
        .set_tools(Some(tools))
        .build()
        .map_err(|e| ToolError::Execution(format!("tool config: {e}")))?;

    let inference = brt::InferenceConfiguration::builder()
        .max_tokens(2000)
        .build();

    let mut messages: Vec<brt::Message> = vec![brt::Message::builder()
        .role(brt::ConversationRole::User)
        .content(brt::ContentBlock::Text(prompt.to_string()))
        .build()
        .map_err(|e| ToolError::Execution(format!("seed message: {e}")))?];

    let mut tools_used: Vec<String> = Vec::new();
    let mut iterations: u32 = 0;
    let answer: String;

    loop {
        iterations += 1;
        if iterations > MAX_ITERATIONS {
            answer = "I stopped after several investigation steps without reaching a final \
                      answer. Try narrowing the question."
                .to_string();
            break;
        }

        let resp = client
            .converse()
            .model_id(&model)
            .set_messages(Some(messages.clone()))
            .system(brt::SystemContentBlock::Text(system.clone()))
            .tool_config(tool_config.clone())
            .inference_config(inference.clone())
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("bedrock converse: {e}")))?;

        let assistant = match resp.output() {
            Some(brt::ConverseOutput::Message(m)) => m.clone(),
            _ => return Err(ToolError::Execution("bedrock returned no message".into())),
        };

        // Record the assistant turn in the running transcript.
        messages.push(
            brt::Message::builder()
                .role(brt::ConversationRole::Assistant)
                .set_content(Some(assistant.content().to_vec()))
                .build()
                .map_err(|e| ToolError::Execution(format!("assistant message: {e}")))?,
        );

        if matches!(resp.stop_reason(), brt::StopReason::ToolUse) {
            let mut results: Vec<brt::ContentBlock> = Vec::new();
            for block in assistant.content() {
                let brt::ContentBlock::ToolUse(tu) = block else {
                    continue;
                };
                let name = tu.name().to_string();
                let args = document_to_json(tu.input());
                tools_used.push(name.clone());

                let (text, is_error) = if READ_ONLY_TOOLS.contains(&name.as_str()) {
                    // Box the recursive future: dispatch_tool -> ask_amos -> dispatch_tool.
                    match Box::pin(dispatch_tool(state, claims, &name, args)).await {
                        Ok(v) => (v.to_string(), false),
                        Err(e) => (format!("error: {}", err_text(e)), true),
                    }
                } else {
                    (
                        format!(
                            "'{name}' is a mutating verb and is not available in read-only \
                             manager mode. Propose it to the user instead."
                        ),
                        true,
                    )
                };

                let mut trb = brt::ToolResultBlock::builder()
                    .tool_use_id(tu.tool_use_id())
                    .content(brt::ToolResultContentBlock::Text(text));
                if is_error {
                    trb = trb.status(brt::ToolResultStatus::Error);
                }
                results
                    .push(brt::ContentBlock::ToolResult(trb.build().map_err(|e| {
                        ToolError::Execution(format!("tool result: {e}"))
                    })?));
            }
            messages.push(
                brt::Message::builder()
                    .role(brt::ConversationRole::User)
                    .set_content(Some(results))
                    .build()
                    .map_err(|e| ToolError::Execution(format!("tool result message: {e}")))?,
            );
            continue;
        }

        // Terminal turn — gather the assistant's text.
        answer = assistant
            .content()
            .iter()
            .filter_map(|b| match b {
                brt::ContentBlock::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        break;
    }

    tools_used.dedup();
    Ok(json!({
        "answer": answer,
        "tools_used": tools_used,
        "iterations": iterations,
    }))
}

fn json_to_document(v: &Value) -> Document {
    match v {
        Value::Null => Document::Null,
        Value::Bool(b) => Document::Bool(*b),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Document::Number(Number::PosInt(u))
            } else if let Some(i) = n.as_i64() {
                Document::Number(Number::NegInt(i))
            } else {
                Document::Number(Number::Float(n.as_f64().unwrap_or(0.0)))
            }
        }
        Value::String(s) => Document::String(s.clone()),
        Value::Array(a) => Document::Array(a.iter().map(json_to_document).collect()),
        Value::Object(o) => {
            let mut m: HashMap<String, Document> = HashMap::new();
            for (k, val) in o {
                m.insert(k.clone(), json_to_document(val));
            }
            Document::Object(m)
        }
    }
}

fn document_to_json(d: &Document) -> Value {
    match d {
        Document::Null => Value::Null,
        Document::Bool(b) => json!(b),
        Document::Number(Number::PosInt(u)) => json!(u),
        Document::Number(Number::NegInt(i)) => json!(i),
        Document::Number(Number::Float(f)) => json!(f),
        Document::String(s) => json!(s),
        Document::Array(a) => Value::Array(a.iter().map(document_to_json).collect()),
        Document::Object(o) => {
            let mut m = serde_json::Map::new();
            for (k, v) in o {
                m.insert(k.clone(), document_to_json(v));
            }
            Value::Object(m)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{document_to_json, json_to_document, READ_ONLY_TOOLS};
    use serde_json::json;

    #[test]
    fn json_document_round_trips() {
        let v = json!({
            "deployment_id": "abc",
            "max_rows": 100,
            "dry_run": false,
            "nested": {"keys": ["a", "b"], "n": -3, "f": 1.5, "nil": null},
        });
        let back = document_to_json(&json_to_document(&v));
        assert_eq!(v, back);
    }

    #[test]
    fn allowlist_is_read_only() {
        // None of the mutating verbs may be in the autonomous toolset.
        for forbidden in [
            "deploy",
            "deploy_app",
            "app_redeploy",
            "app_control",
            "provision_env",
            "teardown_env",
            "db_write",
            "s3_put",
            "s3_delete",
            "build_image",
            "provision_harness",
            "harness_control",
            "set_harness_config",
        ] {
            assert!(
                !READ_ONLY_TOOLS.contains(&forbidden),
                "{forbidden} must not be exposed"
            );
        }
    }
}
