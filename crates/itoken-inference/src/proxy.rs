use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use futures::stream::{BoxStream, StreamExt};
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;
use tracing::{debug, error, warn};
use itoken_core::types::InferenceRequest;

// ─── OpenAI API Types ──────────────────────────────────────────────────────────

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

// ─── Inference Proxy ───────────────────────────────────────────────────────────

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

    /// Proxy an inference request to the local LLM backend via OpenAI-compatible streaming API.
    /// Returns a token stream and a metrics extraction closure.
    pub async fn proxy_query(
        &self,
        req: InferenceRequest,
    ) -> Result<
        (
            BoxStream<'static, Result<String, String>>,
            impl FnOnce() -> (usize, f64), // (token_count, tokens_per_second)
        ),
        String,
    > {
        // Validate request before forwarding
        req.validate().map_err(|e| format!("Request validation failed: {}", e))?;

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

        debug!(endpoint = %endpoint, "Sending streaming request to backend");

        let resp = self.client
            .post(&endpoint)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("Failed to connect to LLM backend at {}: {}", self.backend_url, e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_text = resp.text().await.unwrap_or_default();
            error!(status = %status, error = %err_text, "Backend returned error");
            return Err(format!("LLM backend error ({}): {}", status, err_text));
        }

        let mut stream = resp.bytes_stream();
        let start_time = Instant::now();
        let total_characters = Arc::new(AtomicUsize::new(0));
        let total_tokens = Arc::new(AtomicUsize::new(0));

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
                                    total_chars_stream.fetch_add(content.len(), Ordering::SeqCst);
                                    total_tokens_stream.fetch_add(estimate_token_count(content), Ordering::SeqCst);
                                    yield content.to_string();
                                }
                            }
                        }
                    }
                }
            }
        };

        let boxed_stream = output_stream
            .map(|res| res.map_err(|e: String| e))
            .boxed();

        let get_metrics = move || {
            let elapsed = start_time.elapsed().as_secs_f64();
            let total_chars_val = total_characters.load(Ordering::SeqCst);
            let total_tokens_val = total_tokens.load(Ordering::SeqCst);
            let tokens = if total_tokens_val == 0 && total_chars_val > 0 {
                // Fallback: approximate 4 characters per token
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

// ─── Token Estimation ──────────────────────────────────────────────────────────

/// Approximate BPE token count from text.
/// For English: ~1.3 tokens per word. For CJK: ~1 token per character.
/// This is a billing approximation; exact counts require the actual tokenizer.
fn estimate_token_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }

    let mut count = 0;
    for word in text.split_whitespace() {
        count += 1;
        // Punctuation that BPE typically splits as separate tokens
        let trailing_punct = word.chars().rev().take_while(|c| c.is_ascii_punctuation()).count();
        count += trailing_punct;
    }

    // CJK characters are typically 1 token each
    let cjk_chars = text.chars().filter(|c| is_cjk(*c)).count();
    if cjk_chars > 0 {
        count += cjk_chars;
    }

    count.max(1)
}

fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}' |   // CJK Unified Ideographs
        '\u{3400}'..='\u{4DBF}' |   // CJK Extension A
        '\u{AC00}'..='\u{D7AF}' |   // Hangul Syllables
        '\u{3040}'..='\u{309F}' |   // Hiragana
        '\u{30A0}'..='\u{30FF}'     // Katakana
    )
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_token_count_english() {
        assert_eq!(estimate_token_count("hello world"), 2);
        assert_eq!(estimate_token_count("hello, world!"), 4); // hello + , + world + !
    }

    #[test]
    fn test_estimate_token_count_empty() {
        assert_eq!(estimate_token_count(""), 0);
    }

    #[test]
    fn test_estimate_token_count_cjk() {
        let count = estimate_token_count("안녕하세요");
        assert!(count >= 5, "Korean characters should each count as a token");
    }

    #[test]
    fn test_is_cjk() {
        assert!(is_cjk('한'));
        assert!(is_cjk('字'));
        assert!(!is_cjk('A'));
        assert!(!is_cjk('1'));
    }
}
