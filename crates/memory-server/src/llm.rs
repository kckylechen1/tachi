// llm.rs — LLM & Embedding client for memory server

use async_openai::{
    config::OpenAIConfig,
    types::{
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
        ChatCompletionRequestSystemMessageContent, ChatCompletionRequestUserMessage,
        ChatCompletionRequestUserMessageContent, CreateChatCompletionRequestArgs,
    },
};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;

// LLM and embedding client using Voyage API for embeddings
// and SiliconFlow (OpenAI-compatible) for chat completions
#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    voyage_api_key: String,
    siliconflow_api_key: String,
    siliconflow_model: String,
    summary_model: String,
}

impl LlmClient {
    pub fn new() -> Result<Self, String> {
        let voyage_api_key = std::env::var("VOYAGE_API_KEY")
            .map_err(|_| "VOYAGE_API_KEY environment variable not set".to_string())?;

        let siliconflow_api_key = std::env::var("SILICONFLOW_API_KEY")
            .map_err(|_| "SILICONFLOW_API_KEY environment variable not set".to_string())?;

        let siliconflow_model = std::env::var("SILICONFLOW_MODEL")
            .unwrap_or_else(|_| "Qwen/Qwen2.5-7B-Instruct".to_string());

        let summary_model = std::env::var("SUMMARY_MODEL")
            .unwrap_or_else(|_| siliconflow_model.clone());

        let http = reqwest::Client::new();

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

        // Convert Vec<f64> to Vec<f32>
        let vec: Vec<f32> = embedding
            .iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();

        if vec.len() != 1024 {
            return Err(format!("Expected 1024-dim embedding, got {}", vec.len()));
        }

        Ok(vec)
    }

    /// Call SiliconFlow chat API (OpenAI-compatible)
    pub async fn call_llm(
        &self,
        system: &str,
        user: &str,
        model: Option<&str>,
        temperature: f32,
        max_tokens: u32,
    ) -> Result<String, String> {
        let model = model.unwrap_or(&self.siliconflow_model);

        // Create messages in the correct format for async-openai 0.27
        let system_message = ChatCompletionRequestSystemMessage {
            content: ChatCompletionRequestSystemMessageContent::Text(system.to_string()),
            name: None,
        };
        let user_message = ChatCompletionRequestUserMessage {
            content: ChatCompletionRequestUserMessageContent::Text(user.to_string()),
            name: None,
        };

        let request = CreateChatCompletionRequestArgs::default()
            .model(model)
            .messages([
                ChatCompletionRequestMessage::System(system_message),
                ChatCompletionRequestMessage::User(user_message),
            ])
            .temperature(temperature)
            .max_tokens(max_tokens)
            .build()
            .map_err(|e| format!("Failed to build chat request: {}", e))?;

        // Create a config with custom base URL
        let config = OpenAIConfig::new()
            .with_api_key(&self.siliconflow_api_key)
            .with_api_base("https://api.siliconflow.cn/v1");

        // Use the Client::with_config method
        let client = async_openai::Client::with_config(config);

        let response = client
            .chat()
            .create(request)
            .await
            .map_err(|e| format!("Chat API call failed: {}", e))?;

        let content = response
            .choices
            .first()
            .and_then(|c| c.message.content.as_ref())
            .ok_or("Empty response from chat API")?
            .trim();

        Ok(content.to_string())
    }

    /// Generate L0 summary using SUMMARY_PROMPT
    pub async fn generate_summary(&self, text: &str) -> Result<String, String> {
        match self.call_llm(crate::prompts::SUMMARY_PROMPT, text, None, 0.3, 100).await {
            Ok(summary) => Ok(summary),
            Err(_) => {
                // Fallback to truncation on error
                Ok(text.chars().take(100).collect())
            }
        }
    }

    /// Extract structured facts from text using EXTRACTION_PROMPT
    pub async fn extract_facts(&self, text: &str) -> Result<Vec<Value>, String> {
        let response = self.call_llm(crate::prompts::EXTRACTION_PROMPT, text, None, 0.3, 2000).await?;
        let json_str = Self::strip_code_fence(&response);

        serde_json::from_str(json_str)
            .map_err(|e| format!("Failed to parse facts JSON: {} - response was: {}", e, json_str))
    }

    /// Remove ```json markdown code fences from response
    fn strip_code_fence(text: &str) -> &str {
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
