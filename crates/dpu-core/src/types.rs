use serde::{Deserialize, Serialize};

// ─── Constants ─────────────────────────────────────────────────────────────────
/// 1 iToken = 1,000,000,000 nano-iTokens (9 decimal places, like Gwei)
pub const NANO_PER_ITOKEN: u64 = 1_000_000_000;

/// Maximum allowed receipt age in seconds (5 minutes)
pub const MAX_RECEIPT_AGE_SECS: u64 = 300;

/// Maximum allowed future clock drift in seconds
pub const MAX_FUTURE_DRIFT_SECS: u64 = 30;

/// Maximum allowed prompt size in bytes (128 KB)
pub const MAX_PROMPT_BYTES: usize = 128 * 1024;

/// Maximum allowed max_tokens value
pub const MAX_TOKENS_LIMIT: usize = 8192;

/// Maximum allowed temperature
pub const MAX_TEMPERATURE: f32 = 2.0;

/// Maximum model name length in characters
pub const MAX_MODEL_NAME_LEN: usize = 256;

// ─── Formatting Helpers ────────────────────────────────────────────────────────

/// Format nano-iTokens as human-readable iToken string (e.g., "1.234567890")
pub fn format_itokens(nano: u64) -> String {
    let whole = nano / NANO_PER_ITOKEN;
    let frac = nano % NANO_PER_ITOKEN;
    if frac == 0 {
        format!("{}", whole)
    } else {
        // Trim trailing zeros for cleaner display
        let frac_str = format!("{:09}", frac);
        let trimmed = frac_str.trim_end_matches('0');
        format!("{}.{}", whole, trimmed)
    }
}

/// Parse a human-readable iToken string to nano-iTokens
pub fn parse_itokens(s: &str) -> Result<u64, String> {
    let parts: Vec<&str> = s.split('.').collect();
    match parts.len() {
        1 => {
            let whole: u64 = parts[0].parse().map_err(|e| format!("Invalid amount: {}", e))?;
            whole.checked_mul(NANO_PER_ITOKEN)
                .ok_or_else(|| "Amount overflow".to_string())
        }
        2 => {
            let whole: u64 = parts[0].parse().map_err(|e| format!("Invalid amount: {}", e))?;
            let frac_str = parts[1];
            if frac_str.len() > 9 {
                return Err("Too many decimal places (max 9)".to_string());
            }
            let padded = format!("{:0<9}", frac_str);
            let frac: u64 = padded.parse().map_err(|e| format!("Invalid fraction: {}", e))?;
            whole.checked_mul(NANO_PER_ITOKEN)
                .and_then(|w| w.checked_add(frac))
                .ok_or_else(|| "Amount overflow".to_string())
        }
        _ => Err("Invalid amount format".to_string()),
    }
}

// ─── Model Types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelSpec {
    pub name: String,
    /// Token Quality Weight — nano-iTokens earned per generated token at 1x speed
    pub tqw_nano: u64,
    /// Model parameter count string, e.g. "8B", "70B"
    pub parameters: String,
}

// ─── Inference Types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceRequest {
    pub request_id: String,
    pub prompt: String,
    pub model: String,
    pub max_tokens: Option<usize>,
    pub temperature: f32,
}

impl InferenceRequest {
    /// Validate all fields before processing. Returns Err with reason on failure.
    pub fn validate(&self) -> Result<(), String> {
        if self.prompt.is_empty() {
            return Err("Prompt cannot be empty".to_string());
        }
        if self.prompt.len() > MAX_PROMPT_BYTES {
            return Err(format!(
                "Prompt too large: {} bytes (max {})",
                self.prompt.len(),
                MAX_PROMPT_BYTES
            ));
        }
        if self.model.is_empty() || self.model.len() > MAX_MODEL_NAME_LEN {
            return Err(format!(
                "Model name must be 1-{} characters",
                MAX_MODEL_NAME_LEN
            ));
        }
        if self.temperature < 0.0 || self.temperature > MAX_TEMPERATURE {
            return Err(format!(
                "Temperature must be 0.0-{}, got {}",
                MAX_TEMPERATURE, self.temperature
            ));
        }
        if let Some(max) = self.max_tokens {
            if max == 0 || max > MAX_TOKENS_LIMIT {
                return Err(format!(
                    "max_tokens must be 1-{}, got {}",
                    MAX_TOKENS_LIMIT, max
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResponseChunk {
    pub request_id: String,
    pub text: String,
    pub is_done: bool,
}

// ─── Receipt Types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceReceipt {
    pub receipt_id: String,
    pub client_pubkey: String,
    pub node_pubkey: String,
    pub query_hash: String,
    pub tokens_generated: usize,
    pub tps: f64,
    /// Token Quality Weight in nano-iTokens per generated token at 1x speed
    pub tqw_nano: u64,
    /// Total payment amount in nano-iTokens (exact integer arithmetic)
    pub amount_nano: u64,
    /// Unix timestamp (seconds)
    pub timestamp: u64,
    pub node_signature: Option<String>,
    pub client_signature: Option<String>,
}

impl InferenceReceipt {
    /// Compute the payment amount in nano-iTokens using deterministic integer arithmetic.
    ///
    /// Formula: amount = tokens_generated × tqw_nano × speed_multiplier
    /// The speed multiplier is applied as a rational scaling factor to preserve integer precision.
    pub fn compute_amount(&self, network_median_tps: f64) -> u64 {
        let base_nano = (self.tokens_generated as u64).saturating_mul(self.tqw_nano);
        let multiplier = self.tps_multiplier(network_median_tps);
        // Apply multiplier: multiply base by (multiplier * 1000), then divide by 1000
        // This gives 3 decimal places of precision in the multiplier
        let mult_milli = (multiplier * 1000.0).round() as u64;
        base_nano.saturating_mul(mult_milli) / 1000
    }

    /// Speed multiplier relative to network median TPS.
    /// Formula: (node_tps / median_tps) ^ 0.75, clamped to [0.1, 3.0]
    pub fn tps_multiplier(&self, network_median_tps: f64) -> f64 {
        let median = if network_median_tps > 0.0 {
            network_median_tps
        } else {
            1.0 // Prevent division by zero
        };
        let ratio = self.tps / median;
        ratio.powf(0.75).clamp(0.1, 3.0)
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_itokens_whole() {
        assert_eq!(format_itokens(1_000_000_000), "1");
        assert_eq!(format_itokens(100_000_000_000), "100");
    }

    #[test]
    fn test_format_itokens_fractional() {
        assert_eq!(format_itokens(1_500_000_000), "1.5");
        assert_eq!(format_itokens(123_456_789), "0.123456789");
        assert_eq!(format_itokens(10_000_000), "0.01");
    }

    #[test]
    fn test_parse_itokens_roundtrip() {
        let values = vec![0, 1, 1_000_000_000, 1_500_000_000, 123_456_789];
        for v in values {
            let s = format_itokens(v);
            let parsed = parse_itokens(&s).unwrap();
            assert_eq!(v, parsed, "Roundtrip failed for {}", v);
        }
    }

    #[test]
    fn test_parse_itokens_whole() {
        assert_eq!(parse_itokens("100").unwrap(), 100_000_000_000);
    }

    #[test]
    fn test_compute_amount_deterministic() {
        let r1 = InferenceReceipt {
            receipt_id: "r1".into(),
            client_pubkey: "c".into(),
            node_pubkey: "n".into(),
            query_hash: "h".into(),
            tokens_generated: 100,
            tps: 50.0,
            tqw_nano: 10_000_000, // 0.01 iToken per token
            amount_nano: 0,
            timestamp: 0,
            node_signature: None,
            client_signature: None,
        };
        let a1 = r1.compute_amount(25.0);
        let a2 = r1.compute_amount(25.0);
        assert_eq!(a1, a2, "compute_amount must be deterministic");
        assert!(a1 > 0, "amount must be positive");
    }

    #[test]
    fn test_tps_multiplier_clamping() {
        let r = InferenceReceipt {
            receipt_id: "r".into(),
            client_pubkey: "c".into(),
            node_pubkey: "n".into(),
            query_hash: "h".into(),
            tokens_generated: 1,
            tps: 0.1,
            tqw_nano: 10_000_000,
            amount_nano: 0,
            timestamp: 0,
            node_signature: None,
            client_signature: None,
        };
        // Very slow node → clamp to 0.1
        assert!(r.tps_multiplier(1000.0) >= 0.1);
        // Very fast node → clamp to 3.0
        let r_fast = InferenceReceipt { tps: 100000.0, ..r.clone() };
        assert!(r_fast.tps_multiplier(1.0) <= 3.0);
    }

    #[test]
    fn test_validate_request_good() {
        let req = InferenceRequest {
            request_id: "test".into(),
            prompt: "Hello".into(),
            model: "llama3:8b".into(),
            max_tokens: Some(100),
            temperature: 0.7,
        };
        assert!(req.validate().is_ok());
    }

    #[test]
    fn test_validate_request_empty_prompt() {
        let req = InferenceRequest {
            request_id: "test".into(),
            prompt: "".into(),
            model: "llama3:8b".into(),
            max_tokens: None,
            temperature: 0.0,
        };
        assert!(req.validate().is_err());
    }

    #[test]
    fn test_validate_request_bad_temperature() {
        let req = InferenceRequest {
            request_id: "test".into(),
            prompt: "hi".into(),
            model: "llama3:8b".into(),
            max_tokens: None,
            temperature: -1.0,
        };
        assert!(req.validate().is_err());
    }

    #[test]
    fn test_validate_request_oversized_max_tokens() {
        let req = InferenceRequest {
            request_id: "test".into(),
            prompt: "hi".into(),
            model: "llama3:8b".into(),
            max_tokens: Some(999999),
            temperature: 0.0,
        };
        assert!(req.validate().is_err());
    }
}
