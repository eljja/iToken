use serde::Deserialize;
use std::time::Duration;
use reqwest::Client;
use tracing::{info, warn, debug};
use dpu_core::types::{ModelSpec, NANO_PER_ITOKEN};

// ─── Detected Engine ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DetectedEngine {
    pub name: String,
    pub url: String,
    pub active_models: Vec<ModelSpec>,
}

// ─── API Response Types ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct OpenAIModelsResponse {
    data: Vec<OpenAIModelData>,
}

#[derive(Deserialize)]
struct OpenAIModelData {
    id: String,
}

#[derive(Deserialize)]
struct OllamaTagsResponse {
    models: Vec<OllamaModelData>,
}

#[derive(Deserialize)]
struct OllamaModelData {
    name: String,
}

#[derive(Deserialize)]
struct OllamaShowResponse {
    #[serde(default)]
    parameters: Option<String>,
    #[serde(default)]
    modelinfo: Option<serde_json::Value>,
}

// ─── Port Detector ─────────────────────────────────────────────────────────────

/// Default scan targets: (engine_name, url)
const DEFAULT_TARGETS: &[(&str, &str)] = &[
    ("LM Studio", "http://localhost:1234"),
    ("Ollama", "http://localhost:11434"),
    ("llama.cpp", "http://localhost:8080"),
    ("Kobold.cpp", "http://localhost:5001"),
];

pub struct PortDetector {
    client: Client,
    timeout: Duration,
}

impl PortDetector {
    pub fn new() -> Self {
        Self::with_timeout(Duration::from_millis(1500))
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            client: Client::builder()
                .timeout(timeout)
                .build()
                .unwrap_or_else(|_| Client::new()),
            timeout,
        }
    }

    /// Scan all default ports for running LLM engines.
    pub async fn scan_all(&self) -> Vec<DetectedEngine> {
        let mut engines = Vec::new();

        for (name, url) in DEFAULT_TARGETS {
            match self.probe_engine(name, url).await {
                Ok(models) if !models.is_empty() => {
                    info!(
                        engine = name,
                        url = url,
                        models = models.len(),
                        "Detected running LLM engine"
                    );
                    engines.push(DetectedEngine {
                        name: name.to_string(),
                        url: url.to_string(),
                        active_models: models,
                    });
                }
                Ok(_) => {
                    debug!(engine = name, url = url, "Engine responded but no models loaded");
                }
                Err(_) => {
                    debug!(engine = name, url = url, "Engine not detected");
                }
            }
        }

        engines
    }

    /// Probe a custom user-provided endpoint.
    pub async fn probe_custom(&self, url: &str) -> Result<DetectedEngine, String> {
        let models = self.probe_engine("Custom", url).await?;
        if models.is_empty() {
            return Err("No active models found at endpoint".to_string());
        }
        info!(url = url, models = models.len(), "Detected custom LLM endpoint");
        Ok(DetectedEngine {
            name: "Custom".to_string(),
            url: url.to_string(),
            active_models: models,
        })
    }

    async fn probe_engine(&self, name: &str, url: &str) -> Result<Vec<ModelSpec>, String> {
        // 1. Try standard OpenAI /v1/models endpoint
        let endpoint = format!("{}/v1/models", url);
        if let Ok(resp) = self.client.get(&endpoint).send().await {
            if resp.status().is_success() {
                if let Ok(body) = resp.json::<OpenAIModelsResponse>().await {
                    let mut models = Vec::new();
                    for m in body.data {
                        models.push(ModelSpec {
                            tqw_nano: estimate_tqw_nano(&m.id),
                            parameters: guess_params(&m.id),
                            name: m.id,
                        });
                    }
                    return Ok(models);
                }
            }
        }

        // 2. Ollama-specific /api/tags fallback
        if name == "Ollama" {
            let ollama_endpoint = format!("{}/api/tags", url);
            if let Ok(resp) = self.client.get(&ollama_endpoint).send().await {
                if resp.status().is_success() {
                    if let Ok(body) = resp.json::<OllamaTagsResponse>().await {
                        let mut models = Vec::new();
                        for m in body.models {
                            // Try to get detailed model info from Ollama
                            let tqw = self.query_ollama_model_tqw(url, &m.name).await
                                .unwrap_or_else(|| estimate_tqw_nano(&m.name));
                            let params = self.query_ollama_model_params(url, &m.name).await
                                .unwrap_or_else(|| guess_params(&m.name));
                            models.push(ModelSpec {
                                tqw_nano: tqw,
                                parameters: params,
                                name: m.name,
                            });
                        }
                        return Ok(models);
                    }
                }
            }
        }

        Err("Engine not reachable or no models available".to_string())
    }

    /// Query Ollama's /api/show endpoint for model metadata
    async fn query_ollama_model_tqw(&self, base_url: &str, model: &str) -> Option<u64> {
        let endpoint = format!("{}/api/show", base_url);
        let payload = serde_json::json!({ "name": model });
        let resp = self.client.post(&endpoint).json(&payload).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body: OllamaShowResponse = resp.json().await.ok()?;

        // Try to extract parameter count from model info
        if let Some(info) = &body.modelinfo {
            if let Some(param_count) = info.get("general.parameter_count").and_then(|v| v.as_u64()) {
                return Some(params_to_tqw_nano(param_count));
            }
        }
        None
    }

    async fn query_ollama_model_params(&self, base_url: &str, model: &str) -> Option<String> {
        let endpoint = format!("{}/api/show", base_url);
        let payload = serde_json::json!({ "name": model });
        let resp = self.client.post(&endpoint).json(&payload).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body: OllamaShowResponse = resp.json().await.ok()?;
        if let Some(info) = &body.modelinfo {
            if let Some(count) = info.get("general.parameter_count").and_then(|v| v.as_u64()) {
                return Some(format_param_count(count));
            }
        }
        None
    }
}

// ─── TQW Estimation ────────────────────────────────────────────────────────────

/// Convert actual parameter count to TQW in nano-iTokens per generated token.
fn params_to_tqw_nano(param_count: u64) -> u64 {
    let billions = param_count / 1_000_000_000;
    match billions {
        0..=3 => NANO_PER_ITOKEN / 200,      // ~3B and under: 0.005 iToken/token
        4..=9 => NANO_PER_ITOKEN / 100,       // 4-9B: 0.01 iToken/token
        10..=19 => NANO_PER_ITOKEN * 3 / 100, // 10-19B: 0.03 iToken/token
        20..=39 => NANO_PER_ITOKEN * 6 / 100, // 20-39B: 0.06 iToken/token
        _ => NANO_PER_ITOKEN * 12 / 100,      // 40B+: 0.12 iToken/token
    }
}

fn format_param_count(count: u64) -> String {
    let billions = count / 1_000_000_000;
    if billions > 0 {
        format!("{}B", billions)
    } else {
        let millions = count / 1_000_000;
        format!("{}M", millions)
    }
}

/// Estimate TQW from model name string when metadata is unavailable.
/// This is a fallback — actual metadata should be preferred.
fn estimate_tqw_nano(name: &str) -> u64 {
    let params = guess_params(name);
    match params.as_str() {
        "70B" | "72B" => NANO_PER_ITOKEN * 12 / 100, // 0.12 iToken/token
        "32B" | "34B" => NANO_PER_ITOKEN * 6 / 100,  // 0.06 iToken/token
        "14B" | "13B" => NANO_PER_ITOKEN * 3 / 100,  // 0.03 iToken/token
        _ => NANO_PER_ITOKEN / 100,                    // 0.01 iToken/token (default 8B)
    }
}

fn guess_params(name: &str) -> String {
    let lower = name.to_lowercase();
    if lower.contains("70b") || lower.contains("72b") {
        "70B".to_string()
    } else if lower.contains("32b") || lower.contains("34b") {
        "32B".to_string()
    } else if lower.contains("13b") || lower.contains("14b") {
        "14B".to_string()
    } else if lower.contains("8b") || lower.contains("7b") {
        "8B".to_string()
    } else if lower.contains("3b") || lower.contains("4b") {
        "3B".to_string()
    } else {
        "8B".to_string()
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guess_params_various() {
        assert_eq!(guess_params("llama3-70b-instruct"), "70B");
        assert_eq!(guess_params("qwen2.5:32b"), "32B");
        assert_eq!(guess_params("mistral-7b"), "8B"); // 7b maps to 8B bucket
        assert_eq!(guess_params("phi-3-mini"), "8B"); // Unknown defaults to 8B
    }

    #[test]
    fn test_estimate_tqw_scaling() {
        let tqw_8b = estimate_tqw_nano("llama3:8b");
        let tqw_70b = estimate_tqw_nano("llama3:70b");
        assert!(tqw_70b > tqw_8b, "Larger models should have higher TQW");
    }

    #[test]
    fn test_params_to_tqw_nano() {
        let tqw_3b = params_to_tqw_nano(3_000_000_000);
        let tqw_8b = params_to_tqw_nano(8_000_000_000);
        let tqw_70b = params_to_tqw_nano(70_000_000_000);
        assert!(tqw_3b < tqw_8b);
        assert!(tqw_8b < tqw_70b);
    }

    #[test]
    fn test_format_param_count() {
        assert_eq!(format_param_count(7_000_000_000), "7B");
        assert_eq!(format_param_count(500_000_000), "500M");
    }
}
