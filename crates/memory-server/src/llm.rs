// llm.rs — LLM & Embedding client for memory server
//
// Uses raw reqwest for SiliconFlow (OpenAI-compatible) chat completions
// to support `enable_thinking: false` for Qwen3 models.

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use std::time::Duration;

/// LLM and embedding client using Voyage API for embeddings
/// and SiliconFlow (OpenAI-compatible) for chat completions.
#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    voyage_api_key: String,
    siliconflow_api_key: String,
    siliconflow_model: String,
    summary_model: String,
}

impl LlmClient {
    const MAX_ATTEMPTS: usize = 3;
    const BASE_RETRY_DELAY_MS: u64 = 500;

    pub fn new() -> Result<Self, String> {
        let voyage_api_key = std::env::var("VOYAGE_API_KEY")
            .map_err(|_| "VOYAGE_API_KEY environment variable not set".to_string())?;

        let siliconflow_api_key = std::env::var("SILICONFLOW_API_KEY")
            .map_err(|_| "SILICONFLOW_API_KEY environment variable not set".to_string())?;

        let siliconflow_model = std::env::var("SILICONFLOW_MODEL")
            .unwrap_or_else(|_| "Qwen/Qwen3.5-27B".to_string());

        let summary_model =
            std::env::var("SUMMARY_MODEL").unwrap_or_else(|_| siliconflow_model.clone());

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

        Ok(Self {
            http,
            voyage_api_key,
            siliconflow_api_key,
            siliconflow_model,
            summary_model,
        })
    }

    /// Call Voyage-4 embedding API and return 1024-dim f32 vector
    pub async fn embed_voyage(&self, text: &str, input_type: &str) -> Result<Vec<f32>, String> {
        let body = serde_json::json!({
            "model": "voyage-4",
            "input": [text],
            "input_type": input_type
        });

        let response = self
            .http
            .post("https://api.voyageai.com/v1/embeddings")
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, format!("Bearer {}", self.voyage_api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Voyage API request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(format!("Voyage API error: {} - {}", status, text));
        }

        let json: Value = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse Voyage response: {}", e))?;

        let embedding = json["data"][0]["embedding"]
            .as_array()
            .ok_or("Invalid Voyage response: missing embedding")?;

        let vec: Vec<f32> = embedding
            .iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();

        if vec.len() != 1024 {
            return Err(format!("Expected 1024-dim embedding, got {}", vec.len()));
        }

        Ok(vec)
    }

    /// Call SiliconFlow chat API via raw reqwest.
    /// Includes `enable_thinking: false` for Qwen3 models to prevent
    /// empty `content` when the model puts output in `reasoning_content`.
    pub async fn call_llm(
        &self,
        system: &str,
        user: &str,
        model: Option<&str>,
        temperature: f32,
        max_tokens: u32,
    ) -> Result<String, String> {
        let model = model.unwrap_or(&self.siliconflow_model);

        let body = serde_json::json!({
            "model": model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user}
            ],
            "temperature": temperature,
            "max_tokens": max_tokens,
            "enable_thinking": false
        });

        let mut last_err = String::new();

        for attempt in 1..=Self::MAX_ATTEMPTS {
            let resp = self
                .http
                .post("https://api.siliconflow.cn/v1/chat/completions")
                .header(CONTENT_TYPE, "application/json")
                .header(
                    AUTHORIZATION,
                    format!("Bearer {}", self.siliconflow_api_key),
                )
                .json(&body)
                .send()
                .await;

            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    last_err = format!("HTTP request failed: {e}");
                    if attempt < Self::MAX_ATTEMPTS
                        && (e.is_timeout() || e.is_connect() || e.is_request())
                    {
                        eprintln!(
                            "[llm] transient error (attempt {}/{}): {e}; retrying",
                            attempt,
                            Self::MAX_ATTEMPTS
                        );
                        tokio::time::sleep(Self::retry_delay(attempt)).await;
                        continue;
                    }
                    return Err(last_err);
                }
            };

            let status = resp.status();
            let resp_text = resp
                .text()
                .await
                .unwrap_or_else(|e| format!("<read error: {e}>"));

            // Retry on 429 rate-limit or 5xx server errors
            if status.as_u16() == 429 || status.is_server_error() {
                last_err = format!("API error {status}: {resp_text}");
                if attempt < Self::MAX_ATTEMPTS {
                    eprintln!(
                        "[llm] API error {status} (attempt {}/{}); retrying",
                        attempt,
                        Self::MAX_ATTEMPTS
                    );
                    tokio::time::sleep(Self::retry_delay(attempt)).await;
                    continue;
                }
                return Err(last_err);
            }

            if !status.is_success() {
                return Err(format!("Chat API error {status}: {resp_text}"));
            }

            // Parse JSON response
            let json: Value = serde_json::from_str(&resp_text).map_err(|e| {
                format!("Failed to parse chat response JSON: {e} — raw: {resp_text}")
            })?;

            // Extract content from first choice
            let content = json["choices"]
                .as_array()
                .and_then(|choices| {
                    choices.iter().find_map(|choice| {
                        choice["message"]["content"]
                            .as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                    })
                });

            if let Some(text) = content {
                return Ok(text);
            }

            // Content was empty — build diagnostic info
            let finish_reason = json["choices"][0]["finish_reason"]
                .as_str()
                .unwrap_or("null");
            let usage = json
                .get("usage")
                .map(|u| u.to_string())
                .unwrap_or_else(|| "unknown".to_string());

            last_err = format!(
                "Empty assistant content (finish_reason={finish_reason}, usage={usage}, model={model})"
            );

            if attempt < Self::MAX_ATTEMPTS {
                eprintln!(
                    "[llm] empty content (attempt {}/{}): {last_err}; retrying",
                    attempt,
                    Self::MAX_ATTEMPTS
                );
                tokio::time::sleep(Self::retry_delay(attempt)).await;
                continue;
            }
        }

        Err(last_err)
    }

    /// Generate L0 summary using SUMMARY_PROMPT
    pub async fn generate_summary(&self, text: &str) -> Result<String, String> {
        match self
            .call_llm(
                crate::prompts::SUMMARY_PROMPT,
                text,
                Some(&self.summary_model),
                0.3,
                100,
            )
            .await
        {
            Ok(summary) => Ok(summary),
            Err(_) => {
                // Fallback to truncation on error
                Ok(text.chars().take(100).collect())
            }
        }
    }

    /// Extract structured facts from text using EXTRACTION_PROMPT
    pub async fn extract_facts(&self, text: &str) -> Result<Vec<Value>, String> {
        let response = self
            .call_llm(crate::prompts::EXTRACTION_PROMPT, text, None, 0.3, 2000)
            .await?;
        let json_str = Self::strip_code_fence(&response);

        if json_str.trim().is_empty() {
            return Err("LLM returned empty facts payload after stripping fences".to_string());
        }

        serde_json::from_str(json_str)
            .map_err(|e| format!("Failed to parse facts JSON: {} - response was: {}", e, json_str))
    }

    fn retry_delay(attempt: usize) -> Duration {
        let multiplier = 1u64 << attempt.saturating_sub(1).min(4);
        Duration::from_millis(Self::BASE_RETRY_DELAY_MS * multiplier)
    }

    /// Remove ```json markdown code fences from response
    pub fn strip_code_fence(text: &str) -> &str {
        let text = text.trim();
        if text.starts_with("```json") {
            text[7..].trim()
        } else if text.starts_with("```") {
            text[3..].trim()
        } else {
            text
        }
        .trim_end_matches("```")
        .trim()
    }
}
