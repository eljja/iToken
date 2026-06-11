use serde::{Deserialize, Serialize};
use std::time::Duration;
use reqwest::Client;
use dpu_core::types::ModelSpec;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedEngine {
    pub name: String,
    pub url: String,
    pub active_models: Vec<ModelSpec>,
}

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

pub struct PortDetector {
    client: Client,
}

impl PortDetector {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_millis(500)) // Fast timeout for scanning
                .build()
                .unwrap(),
        }
    }

    pub async fn scan_all(&self) -> Vec<DetectedEngine> {
        let targets = vec![
            ("LM Studio", "http://localhost:1234"),
            ("Ollama", "http://localhost:11434"),
            ("llama.cpp", "http://localhost:8080"),
            ("Kobold.cpp", "http://localhost:5001"),
        ];

        let mut engines = Vec::new();

        for (name, url) in targets {
            if let Ok(models) = self.probe_engine(name, url).await {
                if !models.is_empty() {
                    engines.push(DetectedEngine {
                        name: name.to_string(),
                        url: url.to_string(),
                        active_models: models,
                    });
                }
            }
        }

        engines
    }

    pub async fn probe_custom(&self, url: &str) -> Result<DetectedEngine, String> {
        let models = self.probe_engine("Custom", url).await?;
        if models.is_empty() {
            return Err("No active models found at endpoint".to_string());
        }
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
                            tqw: self.guess_tqw(&m.id),
                            parameters: self.guess_params(&m.id),
                            name: m.id,
                        });
                    }
                    return Ok(models);
                }
            }
        }

        // 2. If it is Ollama, it might only respond to /api/tags if the OpenAI endpoint is disabled
        if name == "Ollama" {
            let ollama_endpoint = format!("{}/api/tags", url);
            if let Ok(resp) = self.client.get(&ollama_endpoint).send().await {
                if resp.status().is_success() {
                    if let Ok(body) = resp.json::<OllamaTagsResponse>().await {
                        let mut models = Vec::new();
                        for m in body.models {
                            models.push(ModelSpec {
                                tqw: self.guess_tqw(&m.name),
                                parameters: self.guess_params(&m.name),
                                name: m.name,
                            });
                        }
                        return Ok(models);
                    }
                }
            }
        }

        Err("Connection failed or model endpoint returned error".to_string())
    }

    fn guess_params(&self, name: &str) -> String {
        let name_lower = name.to_lowercase();
        if name_lower.contains("70b") {
            "70B".to_string()
        } else if name_lower.contains("32b") || name_lower.contains("34b") {
            "32B".to_string()
        } else if name_lower.contains("13b") || name_lower.contains("14b") {
            "14B".to_string()
        } else if name_lower.contains("8b") || name_lower.contains("7b") {
            "8B".to_string()
        } else {
            "8B".to_string() // Default guess
        }
    }

    fn guess_tqw(&self, name: &str) -> f64 {
        // Basic static TQW guesses based on parameter size
        // TQW = Token Quality Weight
        let params = self.guess_params(name);
        match params.as_str() {
            "70B" => 0.12,  // High quality premium model
            "32B" => 0.06,  // Mid-high quality
            "14B" => 0.03,  // Medium quality
            _ => 0.01,      // Small models (8B, 7b)
        }
    }
}
