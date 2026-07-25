//! Estimated spend for a thread's token usage.
//!
//! The rates are published list prices per million tokens, so what comes out is
//! an estimate and never a bill: a plan's included quota, discounts and rounding
//! all live on the vendor's side. The table mirrors the one DevezCode ships so
//! both tools quote the same number for the same session.
//!
//! Updating the table: `.knowledge/토큰사용량-단가-갱신.md`.

use serde_json::Value;

/// Cumulative token counts for a thread, split the way billing is: fresh input,
/// cache writes, cache reads and output each carry their own rate.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct TokenTotals {
    pub input_new: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl TokenTotals {
    /// Reads an app-server `TokenUsageBreakdown`. Its `inputTokens` is the whole
    /// input side and already counts the cached and freshly written tokens, so
    /// those come back out to keep each bucket on its own rate.
    pub fn from_breakdown(breakdown: &Value) -> Self {
        let field = |name: &str| breakdown.get(name).and_then(Value::as_u64).unwrap_or(0);
        let cache_read = field("cachedInputTokens");
        let cache_write = field("cacheWriteInputTokens");
        Self {
            input_new: field("inputTokens")
                .saturating_sub(cache_read)
                .saturating_sub(cache_write),
            cache_write,
            cache_read,
            // `reasoningOutputTokens` is a slice of `outputTokens`, not an extra.
            output: field("outputTokens"),
        }
    }

    pub fn is_empty(self) -> bool {
        self.input_new == 0 && self.cache_write == 0 && self.cache_read == 0 && self.output == 0
    }
}

/// Cache rates as a multiple of the model's input rate — current for both
/// vendors, and the same defaults DevezCode applies.
const CACHE_WRITE_MULTIPLIER: f64 = 1.25;
const CACHE_READ_MULTIPLIER: f64 = 0.1;

/// Per-million-token list prices (input, output), matched on a substring of the
/// model slug. Ordered most specific first: `gpt-5.6-luna` has to be tested
/// before `gpt-5.6`, or the tier would fall through to the generic rate.
const PRICES: &[(&str, f64, f64)] = &[
    ("gpt-5.6-terra", 2.5, 15.0),
    ("gpt-5.6-luna", 1.0, 6.0),
    ("gpt-5.6", 5.0, 30.0),
    ("gpt-5.5", 5.0, 30.0),
    ("gpt-5.3-codex", 1.75, 14.0),
    ("codex", 1.25, 10.0),
    ("gpt-5", 1.25, 10.0),
];

/// Estimated dollars for `totals` at `model`'s rates, or `None` when the model
/// is not in the table — a made-up rate is worse than no figure at all.
pub fn estimate_usd(model: &str, totals: TokenTotals) -> Option<f64> {
    let model = model.to_ascii_lowercase();
    let (_, input_rate, output_rate) = PRICES
        .iter()
        .find(|(slug, _, _)| model.contains(slug))
        .copied()?;
    let input_rate = input_rate / 1_000_000.0;
    let output_rate = output_rate / 1_000_000.0;
    Some(
        totals.input_new as f64 * input_rate
            + totals.cache_write as f64 * input_rate * CACHE_WRITE_MULTIPLIER
            + totals.cache_read as f64 * input_rate * CACHE_READ_MULTIPLIER
            + totals.output as f64 * output_rate,
    )
}

/// Dollars at cent precision. Anything under a cent reads `<$0.01` rather than
/// `$0.00`, which would look like the session was free.
pub fn format_usd(cost: f64) -> String {
    if cost > 0.0 && cost < 0.005 {
        return "<$0.01".to_owned();
    }
    format!("${cost:.2}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn breakdown_splits_cached_and_written_tokens_out_of_the_input_total() {
        let totals = TokenTotals::from_breakdown(&json!({
            "totalTokens": 130_000,
            "inputTokens": 120_000,
            "cachedInputTokens": 90_000,
            "cacheWriteInputTokens": 20_000,
            "outputTokens": 10_000,
            "reasoningOutputTokens": 4_000
        }));

        assert_eq!(
            totals,
            TokenTotals {
                input_new: 10_000,
                cache_write: 20_000,
                cache_read: 90_000,
                output: 10_000
            }
        );
        assert!(!totals.is_empty());
    }

    #[test]
    fn model_tiers_are_matched_before_the_generic_family() {
        let totals = TokenTotals {
            input_new: 1_000_000,
            output: 1_000_000,
            ..Default::default()
        };

        assert_eq!(estimate_usd("gpt-5.6-luna", totals), Some(7.0));
        assert_eq!(estimate_usd("gpt-5.6-terra", totals), Some(17.5));
        assert_eq!(estimate_usd("gpt-5.6-sol", totals), Some(35.0));
        assert_eq!(estimate_usd("gpt-5.3-codex", totals), Some(15.75));
        assert_eq!(estimate_usd("claude-opus-5", totals), None);
    }

    #[test]
    fn cache_buckets_are_discounted_against_the_input_rate() {
        let totals = TokenTotals {
            input_new: 0,
            cache_write: 1_000_000,
            cache_read: 1_000_000,
            output: 0,
        };

        // gpt-5.6 input is $5/M: write ×1.25 = 6.25, read ×0.1 = 0.50.
        assert_eq!(estimate_usd("gpt-5.6-sol", totals), Some(6.75));
    }

    #[test]
    fn figures_stay_short_enough_for_the_composer_rule() {
        assert_eq!(format_usd(0.0), "$0.00");
        assert_eq!(format_usd(0.004), "<$0.01");
        assert_eq!(format_usd(0.42), "$0.42");
        assert_eq!(format_usd(12.5), "$12.50");
    }
}
