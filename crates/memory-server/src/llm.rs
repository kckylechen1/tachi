// llm.rs — LLM & Embedding client for memory server
//
// Uses raw reqwest for OpenAI-compatible chat completions.
// SiliconFlow/Qwen still gets `enable_thinking: false` to avoid empty content.

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use std::time::Duration;

const DEFAULT_CHAT_BASE_URL: &str = "https://api.siliconflow.cn/v1/chat/completions";
const DEFAULT_EXTRACT_MODEL: &str = "Qwen/Qwen3.5-27B";
const DEFAULT_REASONING_MODEL: &str = "Qwen/Qwen3.5-27B";

#[derive(Clone)]
struct ChatLaneConfig {
    base_url: String,
    api_key: String,
    model: String,
}

#[derive(Clone, Copy)]
enum ChatLane {
    Extract,
    Distill,
    Reasoning,
    Summary,
}

/// LLM and embedding client using Voyage API for embeddings
/// and lane-specific OpenAI-compatible chat providers.
#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    voyage_api_key: String,
    extract: ChatLaneConfig,
    distill: ChatLaneConfig,
    reasoning: ChatLaneConfig,
    summary: ChatLaneConfig,
}

impl LlmClient {
    const MAX_ATTEMPTS: usize = 3;
    const BASE_RETRY_DELAY_MS: u64 = 500;

    pub fn new() -> Result<Self, String> {
        let voyage_api_key = std::env::var("VOYAGE_API_KEY")
            .map_err(|_| "VOYAGE_API_KEY environment variable not set".to_string())?;

        let extract = Self::load_lane(
            &["EXTRACT_API_KEY", "SILICONFLOW_API_KEY"],
            &["EXTRACT_BASE_URL", "SILICONFLOW_BASE_URL", "EXTRACTOR_BASE_URL"],
            &["EXTRACT_MODEL", "SILICONFLOW_MODEL", "EXTRACTOR_MODEL"],
            DEFAULT_EXTRACT_MODEL,
        )?;

        let distill = Self::load_lane(
            &["DISTILL_API_KEY", "SUMMARY_API_KEY", "SILICONFLOW_API_KEY"],
            &[
                "DISTILL_BASE_URL",
                "SUMMARY_BASE_URL",
                "SILICONFLOW_BASE_URL",
                "EXTRACTOR_BASE_URL",
            ],
            &[
                "DISTILL_MODEL",
                "SUMMARY_MODEL",
                "SILICONFLOW_MODEL",
                "EXTRACTOR_MODEL",
            ],
            &extract.model,
        )?;

        let reasoning = Self::load_lane(
            &["REASONING_API_KEY", "SILICONFLOW_API_KEY"],
            &[
                "REASONING_BASE_URL",
                "SILICONFLOW_BASE_URL",
                "EXTRACTOR_BASE_URL",
            ],
            &["REASONING_MODEL", "SILICONFLOW_MODEL", "EXTRACTOR_MODEL"],
            DEFAULT_REASONING_MODEL,
        )?;

        let summary = Self::load_lane(
            &[
                "SUMMARY_API_KEY",
                "DISTILL_API_KEY",
                "SILICONFLOW_API_KEY",
            ],
            &[
                "SUMMARY_BASE_URL",
                "DISTILL_BASE_URL",
                "SILICONFLOW_BASE_URL",
                "EXTRACTOR_BASE_URL",
            ],
            &[
                "SUMMARY_MODEL",
                "DISTILL_MODEL",
                "SILICONFLOW_MODEL",
                "EXTRACTOR_MODEL",
            ],
            &distill.model,
        )?;

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

        Ok(Self {
            http,
            voyage_api_key,
            extract,
            distill,
            reasoning,
            summary,
        })
    }

    fn load_lane(
        api_key_envs: &[&str],
        base_url_envs: &[&str],
        model_envs: &[&str],
        default_model: &str,
    ) -> Result<ChatLaneConfig, String> {
        let api_key = Self::first_env(api_key_envs)
            .ok_or_else(|| format!("Missing API key env vars: {}", api_key_envs.join(", ")))?;
        let base_url = Self::first_env(base_url_envs)
            .unwrap_or_else(|| DEFAULT_CHAT_BASE_URL.to_string());
        let model =
            Self::first_env(model_envs).unwrap_or_else(|| default_model.trim().to_string());

        Ok(ChatLaneConfig {
            base_url,
            api_key,
            model,
        })
    }

    fn first_env(keys: &[&str]) -> Option<String> {
        keys.iter().find_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
    }

    fn lane(&self, lane: ChatLane) -> &ChatLaneConfig {
        match lane {
            ChatLane::Extract => &self.extract,
            ChatLane::Distill => &self.distill,
            ChatLane::Reasoning => &self.reasoning,
            ChatLane::Summary => &self.summary,
        }
    }

    fn should_disable_thinking(base_url: &str, model: &str) -> bool {
        base_url.to_ascii_lowercase().contains("siliconflow")
            && model.to_ascii_lowercase().contains("qwen")
    }

    /// Call Voyage-4 embedding API and return 1024-dim f32 vector.
    /// Convenience wrapper around embed_voyage_batch for single-item use.
    #[allow(dead_code)]
    pub async fn embed_voyage(&self, text: &str, input_type: &str) -> Result<Vec<f32>, String> {
        let results = self
            .embed_voyage_batch(&[text.to_string()], input_type)
            .await?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| "Empty batch result".to_string())
    }

    /// Batch call Voyage-4 embedding API. Returns one 1024-dim f32 vector per input text.
    /// Voyage supports up to 128 inputs per request; this method handles chunking internally.
    pub async fn embed_voyage_batch(
        &self,
        texts: &[String],
        input_type: &str,
    ) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        const VOYAGE_MAX_BATCH: usize = 128;
        let mut all_embeddings: Vec<Vec<f32>> = Vec::with_capacity(texts.len());

        for chunk in texts.chunks(VOYAGE_MAX_BATCH) {
            let body = serde_json::json!({
                "model": "voyage-4",
                "input": chunk,
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
                .map_err(|e| format!("Voyage batch API request failed: {}", e))?;

            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                return Err(format!("Voyage batch API error: {} - {}", status, text));
            }

            let json: Value = response
                .json()
                .await
                .map_err(|e| format!("Failed to parse Voyage batch response: {}", e))?;

            let data = json["data"]
                .as_array()
                .ok_or("Invalid Voyage batch response: missing data array")?;

            for item in data {
                let embedding = item["embedding"]
                    .as_array()
                    .ok_or("Invalid Voyage batch response: missing embedding in item")?;

                let vec: Vec<f32> = embedding
                    .iter()
                    .filter_map(|v| v.as_f64().map(|f| f as f32))
                    .collect();

                if vec.len() != 1024 {
                    return Err(format!("Expected 1024-dim embedding, got {}", vec.len()));
                }

                all_embeddings.push(vec);
            }
        }

        if all_embeddings.len() != texts.len() {
            return Err(format!(
                "Voyage batch returned {} embeddings for {} inputs",
                all_embeddings.len(),
                texts.len()
            ));
        }

        Ok(all_embeddings)
    }

    /// Call Voyage rerank API and return (original_index, relevance_score) pairs.
    pub async fn rerank_voyage(
        &self,
        query: &str,
        documents: &[String],
        top_k: usize,
    ) -> Result<Vec<(usize, f64)>, String> {
        if documents.is_empty() {
            return Ok(vec![]);
        }

        let body = serde_json::json!({
            "model": "rerank-2.5",
            "query": query,
            "documents": documents,
            "top_k": top_k.max(1).min(documents.len()),
        });

        let response = self
            .http
            .post("https://api.voyageai.com/v1/rerank")
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, format!("Bearer {}", self.voyage_api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Voyage rerank API request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(format!("Voyage rerank API error: {} - {}", status, text));
        }

        let json: Value = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse Voyage rerank response: {}", e))?;
        let data = json["data"]
            .as_array()
            .ok_or("Invalid Voyage rerank response: missing data array")?;

        let mut out = Vec::with_capacity(data.len());
        for item in data {
            let index = item["index"]
                .as_u64()
                .ok_or("Invalid Voyage rerank response: missing index")?
                as usize;
            let relevance = item["relevance_score"]
                .as_f64()
                .ok_or("Invalid Voyage rerank response: missing relevance_score")?;
            out.push((index, relevance));
        }
        Ok(out)
    }

    /// Backward-compatible generic chat call.
    /// Defaults to the reasoning lane unless a caller uses a lane-specific helper.
    pub async fn call_llm(
        &self,
        system: &str,
        user: &str,
        model: Option<&str>,
        temperature: f32,
        max_tokens: u32,
    ) -> Result<String, String> {
        self.call_lane_llm(ChatLane::Reasoning, system, user, model, temperature, max_tokens)
            .await
    }

    pub async fn call_extract_llm(
        &self,
        system: &str,
        user: &str,
        model: Option<&str>,
        temperature: f32,
        max_tokens: u32,
    ) -> Result<String, String> {
        self.call_lane_llm(ChatLane::Extract, system, user, model, temperature, max_tokens)
            .await
    }

    pub async fn call_distill_llm(
        &self,
        system: &str,
        user: &str,
        model: Option<&str>,
        temperature: f32,
        max_tokens: u32,
    ) -> Result<String, String> {
        self.call_lane_llm(ChatLane::Distill, system, user, model, temperature, max_tokens)
            .await
    }

    pub async fn call_reasoning_llm(
        &self,
        system: &str,
        user: &str,
        model: Option<&str>,
        temperature: f32,
        max_tokens: u32,
    ) -> Result<String, String> {
        self.call_lane_llm(ChatLane::Reasoning, system, user, model, temperature, max_tokens)
            .await
    }

    pub async fn call_summary_llm(
        &self,
        system: &str,
        user: &str,
        model: Option<&str>,
        temperature: f32,
        max_tokens: u32,
    ) -> Result<String, String> {
        self.call_lane_llm(ChatLane::Summary, system, user, model, temperature, max_tokens)
            .await
    }

    async fn call_lane_llm(
        &self,
        lane: ChatLane,
        system: &str,
        user: &str,
        model_override: Option<&str>,
        temperature: f32,
        max_tokens: u32,
    ) -> Result<String, String> {
        let lane_cfg = self.lane(lane);
        let model = model_override.unwrap_or(&lane_cfg.model);

        let mut body = serde_json::json!({
            "model": model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user}
            ],
            "temperature": temperature,
            "max_tokens": max_tokens
        });
        if Self::should_disable_thinking(&lane_cfg.base_url, model) {
            body["enable_thinking"] = Value::Bool(false);
        }

        let mut last_err = String::new();

        for attempt in 1..=Self::MAX_ATTEMPTS {
            let resp = self
                .http
                .post(&lane_cfg.base_url)
                .header(CONTENT_TYPE, "application/json")
                .header(AUTHORIZATION, format!("Bearer {}", lane_cfg.api_key))
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
            let content = json["choices"].as_array().and_then(|choices| {
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
            .call_summary_llm(
                crate::prompts::SUMMARY_PROMPT,
                text,
                None,
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
            .call_extract_llm(crate::prompts::EXTRACTION_PROMPT, text, None, 0.3, 2000)
            .await?;
        let json_str = Self::strip_code_fence(&response);

        if json_str.trim().is_empty() {
            return Err("LLM returned empty facts payload after stripping fences".to_string());
        }

        serde_json::from_str(json_str).map_err(|e| {
            format!(
                "Failed to parse facts JSON: {} - response was: {}",
                e, json_str
            )
        })
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
