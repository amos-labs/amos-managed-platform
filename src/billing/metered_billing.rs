//! Metered billing for shared Bedrock LLM usage.
//!
//! Calculates cost at AWS Bedrock pricing + 3% markup.
//! Costs are tracked in **microcents** (1/100th of a cent) for precision.

/// Markup multiplier applied to base Bedrock pricing.
const MARKUP: f64 = 1.03;

/// Per-model pricing in dollars per million tokens (input, output).
/// Mirror of the catalog in amos-harness/src/routes/settings.rs — when
/// you change one, change the other. Last sync: 2026-05-02 (AWS repriced
/// Haiku up; Anthropic dropped Opus 4.6 to match 4.7).
fn model_pricing(model_id: &str) -> Option<(f64, f64)> {
    // Match on Bedrock model ID prefixes
    let id = model_id.to_lowercase();
    if id.contains("haiku") {
        // Haiku 4.5: AWS Bedrock base $1.00 / $5.00 (was $0.80 / $4.00).
        Some((1.00, 5.00))
    } else if id.contains("opus") {
        // Opus 4.6 and 4.7: base $5.00 / $25.00. 4.6 used to be $15/$75
        // at launch — Anthropic dropped it to match 4.7. Until 2026-05-02
        // we were overcharging customers 3x for shared-Bedrock Opus.
        Some((5.00, 25.00))
    } else if id.contains("sonnet") || id.contains("claude") {
        // Default Claude model = Sonnet pricing.
        Some((3.00, 15.00))
    } else {
        None
    }
}

/// Calculate cost in microcents (hundredths of a cent) for a given model and token counts.
///
/// Returns 0 if the model is unrecognized.
///
/// Formula: (tokens / 1_000_000) × price_per_MTok × markup × 100 (cents) × 100 (microcents)
pub fn calculate_cost_microcents(model_id: &str, tokens_input: u64, tokens_output: u64) -> i64 {
    let Some((input_price, output_price)) = model_pricing(model_id) else {
        return 0;
    };

    let input_cost = (tokens_input as f64 / 1_000_000.0) * input_price * MARKUP * 10_000.0;
    let output_cost = (tokens_output as f64 / 1_000_000.0) * output_price * MARKUP * 10_000.0;

    (input_cost + output_cost).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sonnet_pricing() {
        // 1M input tokens at $3/MTok × 1.03 = $3.09 = 309 cents = 30_900 microcents
        let cost = calculate_cost_microcents("us.anthropic.claude-sonnet-4-6", 1_000_000, 0);
        assert_eq!(cost, 30_900);

        // 1M output tokens at $15/MTok × 1.03 = $15.45 = 1545 cents = 154_500 microcents
        let cost = calculate_cost_microcents("us.anthropic.claude-sonnet-4-6", 0, 1_000_000);
        assert_eq!(cost, 154_500);
    }

    #[test]
    fn haiku_pricing() {
        // 1M input at $1.00 × 1.03 = $1.03 = 103 cents = 10_300 microcents
        let cost =
            calculate_cost_microcents("us.anthropic.claude-haiku-4-5-20251001-v1:0", 1_000_000, 0);
        assert_eq!(cost, 10_300);

        // 1M output at $5.00 × 1.03 = $5.15 = 51_500 microcents
        let cost =
            calculate_cost_microcents("us.anthropic.claude-haiku-4-5-20251001-v1:0", 0, 1_000_000);
        assert_eq!(cost, 51_500);
    }

    #[test]
    fn opus_pricing() {
        // 1M input at $5.00 × 1.03 = $5.15 = 51_500 microcents
        let cost = calculate_cost_microcents("us.anthropic.claude-opus-4-6-v1", 1_000_000, 0);
        assert_eq!(cost, 51_500);

        // 1M output at $25.00 × 1.03 = $25.75 = 257_500 microcents
        let cost = calculate_cost_microcents("us.anthropic.claude-opus-4-6-v1", 0, 1_000_000);
        assert_eq!(cost, 257_500);
    }

    #[test]
    fn combined_input_output() {
        // 500k input + 200k output on Sonnet
        // Input: 0.5 × $3 × 1.03 × 10000 = 15_450
        // Output: 0.2 × $15 × 1.03 × 10000 = 30_900
        // Total: 46_350
        let cost = calculate_cost_microcents("us.anthropic.claude-sonnet-4-6", 500_000, 200_000);
        assert_eq!(cost, 46_350);
    }

    #[test]
    fn unknown_model_returns_zero() {
        let cost = calculate_cost_microcents("unknown-model-v1", 1_000_000, 1_000_000);
        assert_eq!(cost, 0);
    }

    #[test]
    fn zero_tokens_returns_zero() {
        let cost = calculate_cost_microcents("us.anthropic.claude-sonnet-4-6", 0, 0);
        assert_eq!(cost, 0);
    }
}
