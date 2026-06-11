use std::time::Instant;
use futures::stream::{BoxStream, StreamExt};
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;
use dpu_core::types::InferenceRequest;

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Serialize)]
struct OpenAICompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<usize>,
}

pub struct InferenceProxy {
    client: Client,
    backend_url: String,
}

impl InferenceProxy {
    pub fn new(backend_url: String) -> Self {
        Self {
            client: Client::new(),
            backend_url,
        }
    }

    pub async fn proxy_query(
        &self,
        req: InferenceRequest,
    ) -> Result<
        (
            BoxStream<'static, Result<String, String>>,
            impl FnOnce() -> (usize, f64), // Fn to get final metrics: (token_count, tps)
        ),
        String,
    > {
        let endpoint = format!("{}/v1/chat/completions", self.backend_url);
        
        let payload = OpenAICompletionRequest {
            model: req.model,
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: req.prompt,
            }],
            temperature: req.temperature,
            stream: true,
            max_tokens: req.max_tokens,
        };

        let resp = self.client
            .post(&endpoint)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("Failed to connect to local LLM backend: {}", e))?;

        if !resp.status().is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            return Err(format!("Local LLM backend returned error: {}", err_text));
        }

        let mut stream = resp.bytes_stream();
        let start_time = Instant::now();
        let total_characters = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let total_tokens = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let total_chars_stream = total_characters.clone();
        let total_tokens_stream = total_tokens.clone();

        let output_stream = async_stream::try_stream! {
            let mut buffer = String::new();

            while let Some(chunk_result) = stream.next().await {
                let chunk_bytes = chunk_result.map_err(|e| e.to_string())?;
                let chunk_str = String::from_utf8_lossy(&chunk_bytes);
                buffer.push_str(&chunk_str);

                while let Some(line_end) = buffer.find('\n') {
                    let line = buffer.drain(..=line_end).collect::<String>();
                    let trimmed = line.trim();

                    if trimmed.is_empty() {
                        continue;
                    }

                    if trimmed.starts_with("data: ") {
                        let data = &trimmed[6..];
                        if data == "[DONE]" {
                            break;
                        }

                        if let Ok(json_val) = serde_json::from_str::<Value>(data) {
                            if let Some(content) = json_val
                                .get("choices")
                                .and_then(|c| c.get(0))
                                .and_then(|c| c.get("delta"))
                                .and_then(|d| d.get("content"))
                                .and_then(|c| c.as_str())
                            {
                                if !content.is_empty() {
                                    total_chars_stream.fetch_add(content.len(), std::sync::atomic::Ordering::SeqCst);
                                    total_tokens_stream.fetch_add(estimate_token_count(content), std::sync::atomic::Ordering::SeqCst);
                                    yield content.to_string();
                                }
                            }
                        }
                    }
                }
            }
        };

        // Wrap stream in box
        let boxed_stream = output_stream
            .map(|res| res.map_err(|e: String| e))
            .boxed();

        // Metric extraction closure
        let get_metrics = move || {
            let elapsed = start_time.elapsed().as_secs_f64();
            let total_chars_val = total_characters.load(std::sync::atomic::Ordering::SeqCst);
            let total_tokens_val = total_tokens.load(std::sync::atomic::Ordering::SeqCst);
            // Fallback to characters if token estimation is 0
            let tokens = if total_tokens_val == 0 && total_chars_val > 0 {
                (total_chars_val as f64 / 4.0).ceil() as usize
            } else {
                total_tokens_val
            };
            let tps = if elapsed > 0.0 {
                (tokens as f64) / elapsed
            } else {
                0.0
            };
            (tokens, tps)
        };

        Ok((boxed_stream, get_metrics))
    }
}

fn estimate_token_count(text: &str) -> usize {
    // Simple BPE/Tokenization approximation for English:
    // Split by whitespace and count words, and count punctuation marks as separate tokens.
    if text.is_empty() {
        return 0;
    }
    let mut count = 0;
    for word in text.split_whitespace() {
        count += 1;
        // Check for common trailing punctuation
        if word.ends_with('.') || word.ends_with(',') || word.ends_with('!') || word.ends_with('?') || word.ends_with(';') {
            count += 1;
        }
    }
    count.max(1)
}
