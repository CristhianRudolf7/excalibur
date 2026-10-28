//! Reusable outbound provider clients. No application server or CLI is included.
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fmt, time::Duration};
use thiserror::Error;
use url::Url;
use zeroize::Zeroizing;

#[derive(Error, Debug)]
pub enum Error {
    #[error("invalid configuration: {0}")]
    Configuration(&'static str),
    #[error("unsupported operation")]
    Unsupported,
    #[error("provider request failed (status {0})")]
    Http(StatusCode),
    #[error("provider transport failed")]
    Transport,
    #[error("provider returned an invalid response")]
    InvalidResponse,
}

/// Never serializable or printable; the caller owns credential acquisition and rotation.
#[derive(Clone)]
pub struct Secret(Zeroizing<String>);
impl Secret {
    pub fn new(value: impl Into<String>) -> Result<Self, Error> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(Error::Configuration("empty credential"));
        }
        Ok(Self(Zeroizing::new(value)))
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    OpenAi,
    Meta,
    Anthropic,
    GoogleAi,
    Xai,
    DeepSeek,
    Mistral,
    Moonshot,
    MiniMax,
    MiniMaxMedia,
    Alibaba,
    AlibabaMedia,
    Nvidia,
    Groq,
    OpenRouter,
    Zai,
    Cerebras,
    DeepInfra,
    HuggingFace,
    VercelGateway,
    Together,
    TogetherVideo,
    Fireworks,
    Ollama,
    Cohere,
    Replicate,
    Cloudflare,
    Foundry,
    Baseten,
    BasetenManage,
    Bedrock,
    BedrockNative,
    Ai21,
    Perplexity,
    PerplexityAgent,
    Vertex,
    Fal,
    Runway,
    Runpod,
    RunpodServerless,
    Ideogram,
    Luma,
    BlackForest,
    BytePlus,
    Adobe,
    Vidu,
    Pika,
    Kling,
    Modal,
    Recraft,
    Stability,
    LambdaCloud,
    DigitalOcean,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunpodPodAction {
    Start,
    Stop,
    Restart,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunpodJobAction {
    Cancel,
    Retry,
    Stream,
}
impl Provider {
    fn endpoint(self) -> &'static str {
        match self {
            Self::Meta => "https://api.meta.ai/v1/",
            Self::OpenAi => "https://api.openai.com/v1/",
            Self::Anthropic => "https://api.anthropic.com/v1/",
            Self::GoogleAi => "https://generativelanguage.googleapis.com/v1beta/",
            Self::Xai => "https://api.x.ai/v1/",
            Self::DeepSeek => "https://api.deepseek.com/",
            Self::Mistral => "https://api.mistral.ai/v1/",
            Self::Moonshot => "https://api.moonshot.ai/v1/",
            Self::MiniMaxMedia => "https://api.minimax.io/",
            Self::MiniMax => "https://api.minimax.io/v1/",
            Self::AlibabaMedia => "https://dashscope.aliyuncs.com/api/v1/",
            Self::Alibaba => "https://dashscope-intl.aliyuncs.com/compatible-mode/v1/",
            Self::Nvidia => "https://integrate.api.nvidia.com/v1/",
            Self::Groq => "https://api.groq.com/openai/v1/",
            Self::OpenRouter => "https://openrouter.ai/api/v1/",
            Self::Zai => "https://api.z.ai/api/paas/v4/",
            Self::Cerebras => "https://api.cerebras.ai/v1/",
            Self::DeepInfra => "https://api.deepinfra.com/v1/openai/",
            Self::HuggingFace => "https://router.huggingface.co/v1/",
            Self::VercelGateway => "https://ai-gateway.vercel.sh/v1/",
            Self::TogetherVideo => "https://api.together.ai/v2/",
            Self::Together => "https://api.together.ai/v1/",
            Self::Fireworks => "https://api.fireworks.ai/inference/v1/",
            Self::Ollama => "http://127.0.0.1:11434/v1/",
            Self::Cohere => "https://api.cohere.com/v2/",
            Self::Replicate => "https://api.replicate.com/v1/",
            Self::Ai21 => "https://api.ai21.com/studio/v1/",
            Self::Perplexity | Self::PerplexityAgent => "https://api.perplexity.ai/v1/",
            Self::Bedrock | Self::BedrockNative => "https://invalid.example/",
            Self::BasetenManage => "https://api.baseten.co/v1/",
            Self::Baseten => "https://inference.baseten.co/v1/",
            Self::Foundry => "https://invalid.example/",
            Self::Vertex => "https://invalid.example/",
            Self::Cloudflare => "https://api.cloudflare.com/client/v4/",
            Self::Fal => "https://queue.fal.run/",
            Self::Runpod => "https://rest.runpod.io/v1/",
            Self::RunpodServerless => "https://api.runpod.ai/v2/",
            Self::Runway => "https://api.dev.runwayml.com/v1/",
            Self::Stability => "https://api.stability.ai/v2beta/",
            Self::Recraft => "https://external.api.recraft.ai/v1/",
            Self::Kling => "https://api-singapore.klingai.com/v1/",
            Self::Pika => "https://api.dev.pika.art/",
            Self::Vidu => "https://api.vidu.com/ent/v2/",
            Self::Modal => "https://invalid.example/",
            Self::Adobe => "https://firefly-api.adobe.io/",
            Self::BytePlus => "https://ark.ap-southeast.bytepluses.com/api/v3/",
            Self::BlackForest => "https://api.bfl.ai/v1/",
            Self::Luma => "https://agents.lumalabs.ai/v1/",
            Self::Ideogram => "https://api.ideogram.ai/v2/",
            Self::LambdaCloud => "https://cloud.lambda.ai/api/v1/",
            Self::DigitalOcean => "https://api.digitalocean.com/v2/",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}
#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub max_tokens: Option<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatResponse {
    pub text: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// Base URL overrides are intended for private installations and test servers.
/// Credentials are sent only over HTTPS, except for literal loopback addresses.
#[derive(Clone)]
pub struct ProviderClient {
    provider: Provider,
    base: Url,
    credential: Secret,
    client_id: Option<Secret>,
    http: Client,
}
impl fmt::Debug for ProviderClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderClient")
            .field("provider", &self.provider)
            .field("credential", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}
impl ProviderClient {
    /// Call a previously deployed Modal Web Function protected by a proxy token.
    pub fn modal(base: &str, key: Secret, secret: Secret) -> Result<Self, Error> {
        let mut client = Self::with_base_url(Provider::Modal, key, base)?;
        // A Web Function URL is an exact endpoint, not a directory prefix.
        client.base = Url::parse(base).map_err(|_| Error::Configuration("invalid base URL"))?;
        client.client_id = Some(secret);
        Ok(client)
    }
    pub fn adobe(access_token: Secret, client_id: Secret) -> Result<Self, Error> {
        Self::adobe_with_base_url("https://firefly-api.adobe.io/", access_token, client_id)
    }
    pub fn adobe_with_base_url(
        base: &str,
        access_token: Secret,
        client_id: Secret,
    ) -> Result<Self, Error> {
        let mut client = Self::with_base_url(Provider::Adobe, access_token, base)?;
        client.client_id = Some(client_id);
        Ok(client)
    }
    /// Create a regional Bedrock Runtime client using an Amazon Bedrock API key.
    pub fn bedrock(region: &str, key: Secret) -> Result<Self, Error> {
        if region.is_empty()
            || !region
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(Error::Configuration("invalid AWS region"));
        }
        Self::with_base_url(
            Provider::Bedrock,
            key,
            &format!("https://bedrock-runtime.{region}.amazonaws.com/openai/v1/"),
        )
    }
    /// Bedrock Runtime native API, authenticated with a Bedrock API key.
    pub fn bedrock_native(region: &str, key: Secret) -> Result<Self, Error> {
        if region.is_empty()
            || !region
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(Error::Configuration("invalid AWS region"));
        }
        Self::with_base_url(
            Provider::BedrockNative,
            key,
            &format!("https://bedrock-runtime.{region}.amazonaws.com/"),
        )
    }
    /// Supply an Azure Foundry v1 resource URL and key; model is the deployment name.
    pub fn foundry(base: &str, key: Secret) -> Result<Self, Error> {
        Self::with_base_url(Provider::Foundry, key, base)
    }
    /// Caller obtains and refreshes an OAuth access token; no service-account secrets are stored.
    pub fn vertex(project: &str, location: &str, token: Secret) -> Result<Self, Error> {
        if ![project, location].iter().all(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
        }) {
            return Err(Error::Configuration("invalid project or location"));
        }
        let host = if location == "global" {
            "aiplatform.googleapis.com".to_owned()
        } else {
            format!("{location}-aiplatform.googleapis.com")
        };
        Self::with_base_url(
            Provider::Vertex,
            token,
            &format!(
                "https://{host}/v1/projects/{project}/locations/{location}/publishers/google/"
            ),
        )
    }
    pub fn cloudflare(account_id: &str, credential: Secret) -> Result<Self, Error> {
        if account_id.is_empty() || !account_id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(Error::Configuration("invalid account ID"));
        }
        Self::with_base_url(
            Provider::Cloudflare,
            credential,
            &format!("https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1/"),
        )
    }
    pub fn local_ollama() -> Result<Self, Error> {
        Self::new(Provider::Ollama, Secret::new("unused")?)
    }
    pub fn new(provider: Provider, credential: Secret) -> Result<Self, Error> {
        if matches!(
            provider,
            Provider::Cloudflare
                | Provider::Foundry
                | Provider::Vertex
                | Provider::Bedrock
                | Provider::BedrockNative
        ) {
            return Err(Error::Configuration(
                "provider requires a configured resource endpoint",
            ));
        }
        if matches!(provider, Provider::Adobe | Provider::Modal) {
            return Err(Error::Configuration(
                "provider requires an explicit endpoint or additional credentials",
            ));
        }
        Self::with_base_url(provider, credential, provider.endpoint())
    }
    pub fn with_base_url(
        provider: Provider,
        credential: Secret,
        base: &str,
    ) -> Result<Self, Error> {
        let mut base = Url::parse(base).map_err(|_| Error::Configuration("invalid base URL"))?;
        if !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(Error::Configuration(
                "base URL cannot contain credentials, query or fragment",
            ));
        }
        let loopback = matches!(base.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
            || matches!(base.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback());
        if provider == Provider::Ollama && !loopback {
            return Err(Error::Configuration(
                "Ollama local client requires loopback",
            ));
        }
        if base.scheme() != "https" && !(base.scheme() == "http" && loopback) {
            return Err(Error::Configuration("HTTPS required except on loopback"));
        }
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|_| Error::Transport)?;
        Ok(Self {
            provider,
            base,
            credential,
            client_id: None,
            http,
        })
    }
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<reqwest::Response, Error> {
        let url = if self.provider == Provider::Modal && path.is_empty() {
            self.base.clone()
        } else {
            self.base
                .join(path)
                .map_err(|_| Error::Configuration("invalid path"))?
        };
        let mut request = self.http.request(method, url);
        request = match self.provider {
            Provider::Pika => request.header("X-API-Key", self.credential.0.as_str()),
            Provider::Vidu => request.header(
                "authorization",
                format!("Token {}", self.credential.0.as_str()),
            ),
            Provider::Modal => request
                .header("Modal-Key", self.credential.0.as_str())
                .header(
                    "Modal-Secret",
                    self.client_id
                        .as_ref()
                        .ok_or(Error::Configuration("Modal proxy secret required"))?
                        .0
                        .as_str(),
                ),
            Provider::Adobe => request.bearer_auth(self.credential.0.as_str()).header(
                "x-api-key",
                self.client_id
                    .as_ref()
                    .ok_or(Error::Configuration("Adobe client ID required"))?
                    .0
                    .as_str(),
            ),
            Provider::Anthropic => request
                .header("x-api-key", self.credential.0.as_str())
                .header("anthropic-version", "2023-06-01"),
            Provider::AlibabaMedia => request
                .bearer_auth(self.credential.0.as_str())
                .header("X-DashScope-Async", "enable"),
            Provider::Ideogram => request.header("Api-Key", self.credential.0.as_str()),
            Provider::Foundry => request.header("api-key", self.credential.0.as_str()),
            Provider::BlackForest => request.header("x-key", self.credential.0.as_str()),
            Provider::GoogleAi => request.header("x-goog-api-key", self.credential.0.as_str()),
            Provider::Ollama => request,
            Provider::Runway => request
                .bearer_auth(self.credential.0.as_str())
                .header("X-Runway-Version", "2024-11-06"),
            Provider::Fal => request.header(
                "authorization",
                format!("Key {}", self.credential.0.as_str()),
            ),
            _ => request.bearer_auth(self.credential.0.as_str()),
        };
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|_| Error::Transport)?;
        if !response.status().is_success() {
            return Err(Error::Http(response.status()));
        }
        Ok(response)
    }
    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Error> {
        self.request(method, path, body)
            .await?
            .json()
            .await
            .map_err(|_| Error::InvalidResponse)
    }
    fn native_chat_path(&self, model: &str, stream: bool) -> Result<String, Error> {
        if model.trim().is_empty() {
            return Err(Error::Configuration("model required"));
        }
        match self.provider {
            Provider::Anthropic => Ok("messages".into()),
            Provider::Cohere => Ok("chat".into()),
            Provider::Perplexity => Ok("sonar".into()),
            Provider::GoogleAi | Provider::Vertex => {
                if !model
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
                {
                    return Err(Error::Configuration("invalid Google model ID"));
                }
                Ok(format!(
                    "models/{model}:{}",
                    if stream {
                        "streamGenerateContent?alt=sse"
                    } else {
                        "generateContent"
                    }
                ))
            }
            Provider::OpenAi
            | Provider::Meta
            | Provider::Xai
            | Provider::DeepSeek
            | Provider::Mistral
            | Provider::Moonshot
            | Provider::MiniMax
            | Provider::Alibaba
            | Provider::Nvidia
            | Provider::Groq
            | Provider::OpenRouter
            | Provider::Zai
            | Provider::Cerebras
            | Provider::DeepInfra
            | Provider::HuggingFace
            | Provider::VercelGateway
            | Provider::Together
            | Provider::Fireworks
            | Provider::Ollama
            | Provider::Cloudflare
            | Provider::Foundry
            | Provider::Baseten
            | Provider::Bedrock
            | Provider::Ai21 => Ok("chat/completions".into()),
            _ => Err(Error::Unsupported),
        }
    }
    /// Send a provider-native request including tool definitions and tool-result history.
    /// The caller owns schema compatibility and replays the complete returned messages.
    pub async fn native_chat(&self, model: &str, mut body: Value) -> Result<Value, Error> {
        let path = self.native_chat_path(model, false)?;
        let object = body
            .as_object_mut()
            .ok_or(Error::Configuration("request body must be an object"))?;
        if !matches!(self.provider, Provider::GoogleAi | Provider::Vertex) {
            object.insert("model".into(), json!(model));
            object.insert("stream".into(), json!(false));
        }
        self.send(Method::POST, &path, Some(body)).await
    }
    /// Meta Responses supports server-managed or encrypted reasoning replay.
    pub async fn meta_response(&self, mut body: Value) -> Result<Value, Error> {
        if self.provider != Provider::Meta {
            return Err(Error::Unsupported);
        }
        let object = body
            .as_object_mut()
            .ok_or(Error::Configuration("request body must be an object"))?;
        object.insert("stream".into(), json!(false));
        self.send(Method::POST, "responses", Some(body)).await
    }
    pub async fn meta_response_stream(&self, mut body: Value) -> Result<SseStream, Error> {
        if self.provider != Provider::Meta {
            return Err(Error::Unsupported);
        }
        let object = body
            .as_object_mut()
            .ok_or(Error::Configuration("request body must be an object"))?;
        object.insert("stream".into(), json!(true));
        let response = self.request(Method::POST, "responses", Some(body)).await?;
        Ok(SseStream {
            response,
            buffer: Vec::new(),
            done: false,
            saw_terminal: false,
            provider: self.provider,
        })
    }
    /// Stream native SSE events; caller handles provider-specific deltas and tool-call assembly.
    pub async fn stream_native_chat(
        &self,
        model: &str,
        mut body: Value,
    ) -> Result<SseStream, Error> {
        let path = self.native_chat_path(model, true)?;
        let object = body
            .as_object_mut()
            .ok_or(Error::Configuration("request body must be an object"))?;
        if !matches!(self.provider, Provider::GoogleAi | Provider::Vertex) {
            object.insert("model".into(), json!(model));
            object.insert("stream".into(), json!(true));
        }
        let response = self.request(Method::POST, &path, Some(body)).await?;
        Ok(SseStream {
            response,
            buffer: Vec::new(),
            done: false,
            saw_terminal: false,
            provider: self.provider,
        })
    }
    pub async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, Error> {
        if request.model.trim().is_empty()
            || request.messages.is_empty()
            || request.messages.iter().any(|m| m.content.is_empty())
            || request.max_tokens == Some(0)
        {
            return Err(Error::Configuration(
                "model, non-empty messages and positive token limit required",
            ));
        }
        match self.provider {
            Provider::OpenAi
            | Provider::Meta
            | Provider::Xai
            | Provider::DeepSeek
            | Provider::Mistral
            | Provider::Moonshot
            | Provider::MiniMax
            | Provider::Alibaba
            | Provider::Nvidia
            | Provider::Groq
            | Provider::OpenRouter
            | Provider::Zai
            | Provider::Cerebras
            | Provider::DeepInfra
            | Provider::HuggingFace
            | Provider::VercelGateway
            | Provider::Together
            | Provider::Fireworks
            | Provider::Ollama
            | Provider::Cloudflare
            | Provider::Foundry
            | Provider::Baseten
            | Provider::Bedrock
            | Provider::Ai21
            | Provider::Perplexity => {
                let mut body = json!({"model":request.model,"messages":request.messages});
                if let Some(n) = request.max_tokens {
                    body["max_tokens"] = json!(n);
                }
                let v = self
                    .send(
                        Method::POST,
                        if self.provider == Provider::Perplexity {
                            "sonar"
                        } else {
                            "chat/completions"
                        },
                        Some(body),
                    )
                    .await?;
                let text = v
                    .pointer("/choices/0/message/content")
                    .and_then(Value::as_str)
                    .ok_or(Error::InvalidResponse)?
                    .to_owned();
                Ok(ChatResponse {
                    text,
                    input_tokens: v.pointer("/usage/prompt_tokens").and_then(Value::as_u64),
                    output_tokens: v
                        .pointer("/usage/completion_tokens")
                        .and_then(Value::as_u64),
                })
            }
            Provider::Cohere => {
                let mut body =
                    json!({"model":request.model,"messages":request.messages,"stream":false});
                if let Some(n) = request.max_tokens {
                    body["max_tokens"] = json!(n);
                }
                let v = self.send(Method::POST, "chat", Some(body)).await?;
                let parts = v
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .ok_or(Error::InvalidResponse)?;
                let text = parts
                    .iter()
                    .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("");
                if text.is_empty() {
                    return Err(Error::InvalidResponse);
                }
                Ok(ChatResponse {
                    text,
                    input_tokens: v
                        .pointer("/usage/tokens/input_tokens")
                        .and_then(Value::as_u64),
                    output_tokens: v
                        .pointer("/usage/tokens/output_tokens")
                        .and_then(Value::as_u64),
                })
            }
            Provider::Anthropic => {
                let mut system = Vec::new();
                let mut messages = Vec::new();
                for msg in &request.messages {
                    if msg.role == Role::System {
                        system.push(msg.content.as_str());
                    } else {
                        messages.push(json!({"role":msg.role,"content":msg.content}));
                    }
                }
                if messages.is_empty() {
                    return Err(Error::Configuration(
                        "Anthropic requires a non-system message",
                    ));
                }
                let mut body = json!({"model":request.model,"messages":messages,"max_tokens":request.max_tokens.ok_or(Error::Configuration("Anthropic requires max_tokens"))?});
                if !system.is_empty() {
                    body["system"] = json!(system.join("\n\n"));
                }
                let v = self.send(Method::POST, "messages", Some(body)).await?;
                let parts = v
                    .get("content")
                    .and_then(Value::as_array)
                    .ok_or(Error::InvalidResponse)?;
                let text = parts
                    .iter()
                    .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("");
                if text.is_empty() {
                    return Err(Error::InvalidResponse);
                }
                Ok(ChatResponse {
                    text,
                    input_tokens: v.pointer("/usage/input_tokens").and_then(Value::as_u64),
                    output_tokens: v.pointer("/usage/output_tokens").and_then(Value::as_u64),
                })
            }
            Provider::GoogleAi | Provider::Vertex => {
                let mut system = Vec::new();
                let mut contents = Vec::new();
                for msg in &request.messages {
                    if msg.role == Role::System {
                        system.push(msg.content.as_str());
                    } else {
                        contents.push(json!({"role":if msg.role == Role::Assistant {"model"} else {"user"},"parts":[{"text":msg.content}]}));
                    }
                }
                if contents.is_empty() {
                    return Err(Error::Configuration("Google requires a non-system message"));
                }
                // Restrict model identifiers to prevent URL or path injection.
                if !request
                    .model
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
                {
                    return Err(Error::Configuration("invalid Google model ID"));
                }
                let mut body = json!({"contents":contents});
                if !system.is_empty() {
                    body["systemInstruction"] = json!({"parts":[{"text":system.join("\n\n")}]});
                }
                if let Some(n) = request.max_tokens {
                    body["generationConfig"] = json!({"maxOutputTokens":n});
                }
                let v = self
                    .send(
                        Method::POST,
                        &format!("models/{}:generateContent", request.model),
                        Some(body),
                    )
                    .await?;
                let parts = v
                    .pointer("/candidates/0/content/parts")
                    .and_then(Value::as_array)
                    .ok_or(Error::InvalidResponse)?;
                let text = parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("");
                if text.is_empty() {
                    return Err(Error::InvalidResponse);
                }
                Ok(ChatResponse {
                    text,
                    input_tokens: v
                        .pointer("/usageMetadata/promptTokenCount")
                        .and_then(Value::as_u64),
                    output_tokens: v
                        .pointer("/usageMetadata/candidatesTokenCount")
                        .and_then(Value::as_u64),
                })
            }
            _ => Err(Error::Unsupported),
        }
    }
    /// Returns the provider's image response, including URL or base64 payload.
    pub async fn generate_image(&self, model: &str, prompt: &str) -> Result<Value, Error> {
        if !matches!(
            self.provider,
            Provider::OpenAi | Provider::Meta | Provider::Xai | Provider::Together
        ) {
            return Err(Error::Unsupported);
        }
        if model.trim().is_empty() || prompt.trim().is_empty() {
            return Err(Error::Configuration("model and prompt required"));
        }
        let v = self
            .send(
                Method::POST,
                "images/generations",
                Some(json!({"model":model,"prompt":prompt})),
            )
            .await?;
        if v.get("data")
            .and_then(Value::as_array)
            .is_none_or(|a| a.is_empty())
        {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn together_video_submit(&self, body: Value) -> Result<Value, Error> {
        if self.provider != Provider::TogetherVideo {
            return Err(Error::Unsupported);
        }
        if !body.is_object() {
            return Err(Error::Configuration("request body must be an object"));
        }
        let v = self.send(Method::POST, "videos", Some(body)).await?;
        if v.get("id").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn together_video_status(&self, id: &str) -> Result<Value, Error> {
        if self.provider != Provider::TogetherVideo {
            return Err(Error::Unsupported);
        }
        if !safe_id(id) {
            return Err(Error::Configuration("invalid video ID"));
        }
        self.send(Method::GET, &format!("videos/{id}"), None).await
    }
    /// Submit a Vertex video prediction using the configured project and region.
    pub async fn vertex_video_submit(&self, model: &str, body: Value) -> Result<Value, Error> {
        if self.provider != Provider::Vertex {
            return Err(Error::Unsupported);
        }
        if !safe_model(model) || !body.is_object() {
            return Err(Error::Configuration("invalid model or body"));
        }
        let v = self
            .send(
                Method::POST,
                &format!("models/{model}:predictLongRunning"),
                Some(body),
            )
            .await?;
        if v.get("name").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn vertex_video_status(&self, model: &str, operation: &str) -> Result<Value, Error> {
        if self.provider != Provider::Vertex {
            return Err(Error::Unsupported);
        }
        if !safe_model(model)
            || !operation.starts_with("projects/")
            || !safe_relative_path(operation)
        {
            return Err(Error::Configuration("invalid model or operation name"));
        }
        self.send(
            Method::POST,
            &format!("models/{model}:fetchPredictOperation"),
            Some(json!({"operationName":operation})),
        )
        .await
    }
    /// Start a Gemini API long-running video prediction with the provider-specific body.
    pub async fn google_video_submit(&self, model: &str, body: Value) -> Result<Value, Error> {
        if self.provider != Provider::GoogleAi {
            return Err(Error::Unsupported);
        }
        if model.is_empty()
            || !model
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            || !body.is_object()
        {
            return Err(Error::Configuration("invalid model or body"));
        }
        let v = self
            .send(
                Method::POST,
                &format!("models/{model}:predictLongRunning"),
                Some(body),
            )
            .await?;
        if v.get("name").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn google_video_status(&self, operation: &str) -> Result<Value, Error> {
        if self.provider != Provider::GoogleAi {
            return Err(Error::Unsupported);
        }
        if !safe_relative_path(operation)
            || !operation.starts_with("models/")
            || !operation.contains("/operations/")
        {
            return Err(Error::Configuration("invalid operation name"));
        }
        self.send(Method::GET, operation, None).await
    }
    /// Start a video job; submission can be billable and is never retried automatically.
    pub async fn start_video(&self, model: &str, prompt: &str) -> Result<String, Error> {
        if self.provider != Provider::Xai {
            return Err(Error::Unsupported);
        }
        if model.trim().is_empty() || prompt.trim().is_empty() {
            return Err(Error::Configuration("model and prompt required"));
        }
        let v = self
            .send(
                Method::POST,
                "videos/generations",
                Some(json!({"model":model,"prompt":prompt})),
            )
            .await?;
        v.get("request_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or(Error::InvalidResponse)
    }
    /// Query a job once. The caller controls polling and must handle pending, failed and expired states.
    pub async fn video_status(&self, id: &str) -> Result<Value, Error> {
        if self.provider != Provider::Xai {
            return Err(Error::Unsupported);
        }
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
        {
            return Err(Error::Configuration("invalid request ID"));
        }
        let v = self
            .send(Method::GET, &format!("videos/{id}"), None)
            .await?;
        if v.get("status").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    /// Submit a Replicate community model version using its model-specific input schema.
    pub async fn replicate_predict(&self, version: &str, input: Value) -> Result<Value, Error> {
        if self.provider != Provider::Replicate {
            return Err(Error::Unsupported);
        }
        if version.trim().is_empty() || !input.is_object() {
            return Err(Error::Configuration("version and input object required"));
        }
        let v = self
            .send(
                Method::POST,
                "predictions",
                Some(json!({"version":version,"input":input})),
            )
            .await?;
        if v.get("id").and_then(Value::as_str).is_none()
            || v.get("status").and_then(Value::as_str).is_none()
        {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn replicate_status(&self, id: &str) -> Result<Value, Error> {
        if self.provider != Provider::Replicate {
            return Err(Error::Unsupported);
        }
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
        {
            return Err(Error::Configuration("invalid prediction ID"));
        }
        let v = self
            .send(Method::GET, &format!("predictions/{id}"), None)
            .await?;
        if v.get("status").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    /// Submit to an official model or an existing deployment on Replicate.
    pub async fn replicate_named_prediction(
        &self,
        deployment: bool,
        owner: &str,
        name: &str,
        input: Value,
    ) -> Result<Value, Error> {
        if self.provider != Provider::Replicate {
            return Err(Error::Unsupported);
        }
        if !safe_id(owner) || !safe_id(name) || !input.is_object() {
            return Err(Error::Configuration("invalid owner, name or input"));
        }
        let kind = if deployment { "deployments" } else { "models" };
        let result = self
            .send(
                Method::POST,
                &format!("{kind}/{owner}/{name}/predictions"),
                Some(json!({"input":input})),
            )
            .await?;
        if result.get("id").and_then(Value::as_str).is_none()
            || result.get("status").and_then(Value::as_str).is_none()
        {
            return Err(Error::InvalidResponse);
        }
        Ok(result)
    }
    pub async fn replicate_cancel(&self, id: &str) -> Result<Value, Error> {
        if self.provider != Provider::Replicate {
            return Err(Error::Unsupported);
        }
        if !safe_id(id) {
            return Err(Error::Configuration("invalid prediction ID"));
        }
        self.send(Method::POST, &format!("predictions/{id}/cancel"), None)
            .await
    }
    /// Consume the stream URL returned in a prediction, restricted to Replicate hosts.
    pub async fn replicate_stream(&self, stream_url: &str) -> Result<SseStream, Error> {
        if self.provider != Provider::Replicate {
            return Err(Error::Unsupported);
        }
        let url = Url::parse(stream_url).map_err(|_| Error::Configuration("invalid stream URL"))?;
        let allowed = url.scheme() == "https"
            && matches!(
                url.host_str(),
                Some("stream.replicate.com" | "streaming.api.replicate.com")
            );
        let loopback_mock = self.base.scheme() == "http"
            && self.base.host() == url.host()
            && self.base.port_or_known_default() == url.port_or_known_default()
            && url.scheme() == "http";
        if !(allowed || loopback_mock)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::Configuration("untrusted stream URL"));
        }
        let response = self
            .http
            .get(url)
            .bearer_auth(self.credential.0.as_str())
            .header("Accept", "text/event-stream")
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        if !response.status().is_success() {
            return Err(Error::Http(response.status()));
        }
        Ok(SseStream {
            response,
            buffer: Vec::new(),
            done: false,
            saw_terminal: false,
            provider: self.provider,
        })
    }
    /// Send an Agent API request; the native body can contain tools and tool results.
    pub async fn perplexity_agent(&self, mut body: Value) -> Result<Value, Error> {
        if self.provider != Provider::PerplexityAgent {
            return Err(Error::Unsupported);
        }
        let object = body
            .as_object_mut()
            .ok_or(Error::Configuration("request body must be an object"))?;
        object.insert("stream".into(), json!(false));
        self.send(Method::POST, "agent", Some(body)).await
    }
    pub async fn perplexity_agent_stream(&self, mut body: Value) -> Result<SseStream, Error> {
        if self.provider != Provider::PerplexityAgent {
            return Err(Error::Unsupported);
        }
        let object = body
            .as_object_mut()
            .ok_or(Error::Configuration("request body must be an object"))?;
        object.insert("stream".into(), json!(true));
        let response = self.request(Method::POST, "agent", Some(body)).await?;
        Ok(SseStream {
            response,
            buffer: Vec::new(),
            done: false,
            saw_terminal: false,
            provider: self.provider,
        })
    }
    /// Background cancellation is asynchronous; poll until the terminal status.
    pub async fn perplexity_agent_status(&self, id: &str, cancel: bool) -> Result<Value, Error> {
        if self.provider != Provider::PerplexityAgent {
            return Err(Error::Unsupported);
        }
        if !safe_id(id) {
            return Err(Error::Configuration("invalid response ID"));
        }
        let path = format!("agent/{id}{}", if cancel { "/cancel" } else { "" });
        self.send(if cancel { Method::POST } else { Method::GET }, &path, None)
            .await
    }
    /// Send a native Converse body including tool configuration and results.
    pub async fn bedrock_converse(&self, model: &str, body: Value) -> Result<Value, Error> {
        if self.provider != Provider::BedrockNative {
            return Err(Error::Unsupported);
        }
        // Accept common model and inference-profile IDs; ARN-shaped paths require separate encoding.
        if model.is_empty()
            || !model
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c))
            || !body.is_object()
        {
            return Err(Error::Configuration("invalid model ID or body"));
        }
        self.send(Method::POST, &format!("model/{model}/converse"), Some(body))
            .await
    }
    /// Read native AWS event-stream frames; the caller assembles tool and reasoning deltas.
    pub async fn bedrock_converse_stream(
        &self,
        model: &str,
        body: Value,
    ) -> Result<BedrockStream, Error> {
        if self.provider != Provider::BedrockNative {
            return Err(Error::Unsupported);
        }
        if model.is_empty()
            || !model
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c))
            || !body.is_object()
        {
            return Err(Error::Configuration("invalid model ID or body"));
        }
        let response = self
            .request(
                Method::POST,
                &format!("model/{model}/converse-stream"),
                Some(body),
            )
            .await?;
        Ok(BedrockStream {
            response,
            buffer: Vec::new(),
            saw_stop: false,
            done: false,
        })
    }
    /// Submit Kling image or text-to-video generation with an API key.
    pub async fn kling_submit(&self, op: KlingOperation, body: Value) -> Result<Value, Error> {
        if self.provider != Provider::Kling {
            return Err(Error::Unsupported);
        }
        if !body.is_object() {
            return Err(Error::Configuration("request body must be an object"));
        }
        let v = self.send(Method::POST, op.path(), Some(body)).await?;
        if v.get("code").and_then(Value::as_i64) != Some(0)
            || v.pointer("/data/task_id").and_then(Value::as_str).is_none()
        {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn kling_status(&self, op: KlingOperation, id: &str) -> Result<Value, Error> {
        if self.provider != Provider::Kling {
            return Err(Error::Unsupported);
        }
        if !safe_id(id) {
            return Err(Error::Configuration("invalid task ID"));
        }
        let v = self
            .send(Method::GET, &format!("{}/{id}", op.path()), None)
            .await?;
        if v.get("code").and_then(Value::as_i64) != Some(0)
            || v.pointer("/data/task_status")
                .and_then(Value::as_str)
                .is_none()
        {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    /// Activate an existing Baseten deployment; it may begin incurring charges.
    pub async fn baseten_activate(&self, model: &str, deployment: &str) -> Result<(), Error> {
        if self.provider != Provider::BasetenManage {
            return Err(Error::Unsupported);
        }
        if !safe_id(model) || !safe_id(deployment) {
            return Err(Error::Configuration("invalid model or deployment ID"));
        }
        let v = self
            .send(
                Method::POST,
                &format!("models/{model}/deployments/{deployment}/activate"),
                None,
            )
            .await?;
        if v.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(Error::InvalidResponse);
        }
        Ok(())
    }
    /// Query an existing Baseten deployment, including its lifecycle status.
    pub async fn baseten_deployment(&self, model: &str, deployment: &str) -> Result<Value, Error> {
        if self.provider != Provider::BasetenManage {
            return Err(Error::Unsupported);
        }
        if !safe_id(model) || !safe_id(deployment) {
            return Err(Error::Configuration("invalid model or deployment ID"));
        }
        self.send(
            Method::GET,
            &format!("models/{model}/deployments/{deployment}"),
            None,
        )
        .await
    }
    /// Deactivate an existing Baseten deployment to release its replicas.
    pub async fn baseten_deactivate(&self, model: &str, deployment: &str) -> Result<(), Error> {
        if self.provider != Provider::BasetenManage {
            return Err(Error::Unsupported);
        }
        if !safe_id(model) || !safe_id(deployment) {
            return Err(Error::Configuration("invalid model or deployment ID"));
        }
        let v = self
            .send(
                Method::POST,
                &format!("models/{model}/deployments/{deployment}/deactivate"),
                None,
            )
            .await?;
        if v.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(Error::InvalidResponse);
        }
        Ok(())
    }
    /// Create a model and deployment from an accessible Model Labs listing.
    pub async fn baseten_deploy_listing(
        &self,
        team: Option<&str>,
        lab: &str,
        listing: &str,
        name: Option<&str>,
    ) -> Result<Value, Error> {
        if self.provider != Provider::BasetenManage {
            return Err(Error::Unsupported);
        }
        if lab.trim().is_empty()
            || listing.trim().is_empty()
            || team.is_some_and(|id| !safe_id(id))
            || name.is_some_and(|n| n.trim().is_empty())
        {
            return Err(Error::Configuration("invalid listing or team"));
        }
        let path = team.map_or_else(|| "models".to_string(), |id| format!("teams/{id}/models"));
        let mut source = json!({"kind":"library_listing", "lab_display_name":lab, "user_defined_listing_id":listing});
        if let Some(name) = name {
            source["deployed_model_name"] = json!(name);
        }
        self.send(Method::POST, &path, Some(json!({"source":source})))
            .await
    }
    /// Submit a Pika catalog operation using its documented model-specific body.
    /// Caller must verify the live catalog and operation schema before submitting.
    pub async fn pika_submit(&self, operation: &str, body: Value) -> Result<Value, Error> {
        if self.provider != Provider::Pika {
            return Err(Error::Unsupported);
        }
        if !safe_relative_path(operation) || operation.split('/').count() != 3 || !body.is_object()
        {
            return Err(Error::Configuration("invalid operation or input"));
        }
        let v = self
            .send(Method::POST, &format!("v1/media/{operation}"), Some(body))
            .await?;
        if v.get("id").and_then(Value::as_str).is_none()
            || v.get("status").and_then(Value::as_str).is_none()
        {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn pika_status(&self, id: &str) -> Result<Value, Error> {
        if self.provider != Provider::Pika {
            return Err(Error::Unsupported);
        }
        if !safe_id(id) {
            return Err(Error::Configuration("invalid job ID"));
        }
        let v = self
            .send(Method::GET, &format!("v1/media/jobs/{id}"), None)
            .await?;
        if v.get("status").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    /// Invoke a deployed Modal Web Function with its application-defined JSON contract.
    pub async fn modal_invoke(&self, body: Value) -> Result<Value, Error> {
        if self.provider != Provider::Modal {
            return Err(Error::Unsupported);
        }
        if !body.is_object() {
            return Err(Error::Configuration("request body must be an object"));
        }
        self.send(Method::POST, "", Some(body)).await
    }
    pub async fn vidu_submit(&self, body: Value) -> Result<Value, Error> {
        if self.provider != Provider::Vidu {
            return Err(Error::Unsupported);
        }
        if !body.is_object() {
            return Err(Error::Configuration("request body must be an object"));
        }
        let v = self.send(Method::POST, "text2video", Some(body)).await?;
        if v.get("task_id").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn vidu_status(&self, id: &str) -> Result<Value, Error> {
        if self.provider != Provider::Vidu {
            return Err(Error::Unsupported);
        }
        if !safe_id(id) {
            return Err(Error::Configuration("invalid task ID"));
        }
        let v = self
            .send(Method::GET, &format!("tasks/{id}/creations"), None)
            .await?;
        if v.get("state").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    /// Submit and poll an Adobe Firefly image task; caller refreshes the OAuth token.
    pub async fn adobe_image_submit(&self, body: Value) -> Result<Value, Error> {
        if self.provider != Provider::Adobe {
            return Err(Error::Unsupported);
        }
        if !body.is_object() {
            return Err(Error::Configuration("request body must be an object"));
        }
        let v = self
            .send(Method::POST, "v3/images/generate-async", Some(body))
            .await?;
        if v.get("jobId").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn adobe_status(&self, id: &str) -> Result<Value, Error> {
        if self.provider != Provider::Adobe {
            return Err(Error::Unsupported);
        }
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_:".contains(c))
        {
            return Err(Error::Configuration("invalid job ID"));
        }
        let v = self
            .send(Method::GET, &format!("v3/status/{id}"), None)
            .await?;
        if v.get("status").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn byteplus_image(&self, body: Value) -> Result<Value, Error> {
        if self.provider != Provider::BytePlus {
            return Err(Error::Unsupported);
        }
        if !body.is_object() {
            return Err(Error::Configuration("request body must be an object"));
        }
        let v = self
            .send(Method::POST, "images/generations", Some(body))
            .await?;
        if !v.get("data").is_some_and(Value::is_array) {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn byteplus_video_submit(&self, body: Value) -> Result<Value, Error> {
        if self.provider != Provider::BytePlus {
            return Err(Error::Unsupported);
        }
        if !body.is_object() {
            return Err(Error::Configuration("request body must be an object"));
        }
        let v = self
            .send(Method::POST, "contents/generations/tasks", Some(body))
            .await?;
        if v.get("id").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn byteplus_video_status(&self, id: &str) -> Result<Value, Error> {
        if self.provider != Provider::BytePlus {
            return Err(Error::Unsupported);
        }
        if !safe_id(id) {
            return Err(Error::Configuration("invalid task ID"));
        }
        let v = self
            .send(
                Method::GET,
                &format!("contents/generations/tasks/{id}"),
                None,
            )
            .await?;
        if v.get("status").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    /// MiniMax text-to-image generation with the model-specific JSON request.
    pub async fn minimax_image(&self, body: Value) -> Result<Value, Error> {
        if self.provider != Provider::MiniMaxMedia {
            return Err(Error::Unsupported);
        }
        if !body.is_object() {
            return Err(Error::Configuration("request body must be an object"));
        }
        let v = self
            .send(Method::POST, "v1/image_generation", Some(body))
            .await?;
        if v.pointer("/base_resp/status_code").and_then(Value::as_i64) != Some(0)
            || !v.get("data").is_some_and(Value::is_object)
        {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn minimax_video_submit(&self, body: Value) -> Result<Value, Error> {
        if self.provider != Provider::MiniMaxMedia {
            return Err(Error::Unsupported);
        }
        if !body.is_object() {
            return Err(Error::Configuration("request body must be an object"));
        }
        let v = self
            .send(Method::POST, "v2/video_generation", Some(body))
            .await?;
        if v.get("task_id").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn minimax_video_status(&self, id: &str) -> Result<Value, Error> {
        if self.provider != Provider::MiniMaxMedia {
            return Err(Error::Unsupported);
        }
        if !safe_id(id) {
            return Err(Error::Configuration("invalid task ID"));
        }
        self.send(
            Method::GET,
            &format!("v2/query/video_generation/{id}"),
            None,
        )
        .await
    }
    /// Generate a Stable Image Core image as base64 JSON, preserving moderation metadata.
    pub async fn stability_image(&self, prompt: &str) -> Result<Value, Error> {
        if self.provider != Provider::Stability {
            return Err(Error::Unsupported);
        }
        if prompt.trim().is_empty() {
            return Err(Error::Configuration("prompt required"));
        }
        let url = self
            .base
            .join("stable-image/generate/core")
            .map_err(|_| Error::Configuration("invalid path"))?;
        let form = reqwest::multipart::Form::new().text("prompt", prompt.to_owned());
        let response = self
            .http
            .post(url)
            .bearer_auth(self.credential.0.as_str())
            .header("Accept", "application/json")
            .multipart(form)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        if !response.status().is_success() {
            return Err(Error::Http(response.status()));
        }
        let v: Value = response.json().await.map_err(|_| Error::InvalidResponse)?;
        if v.get("finish_reason").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    /// Generate a Recraft image from a prompt using its multipart API.
    pub async fn recraft_image(&self, prompt: &str) -> Result<Value, Error> {
        if self.provider != Provider::Recraft {
            return Err(Error::Unsupported);
        }
        if prompt.trim().is_empty() {
            return Err(Error::Configuration("prompt required"));
        }
        let url = self
            .base
            .join("images/generations")
            .map_err(|_| Error::Configuration("invalid path"))?;
        let form = reqwest::multipart::Form::new().text("prompt", prompt.to_owned());
        let response = self
            .http
            .post(url)
            .bearer_auth(self.credential.0.as_str())
            .multipart(form)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        if !response.status().is_success() {
            return Err(Error::Http(response.status()));
        }
        let v: Value = response.json().await.map_err(|_| Error::InvalidResponse)?;
        if !v.get("data").is_some_and(Value::is_array) {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    /// Submit a BFL image generation to a documented model endpoint.
    pub async fn bfl_submit(&self, model: &str, body: Value) -> Result<Value, Error> {
        if self.provider != Provider::BlackForest {
            return Err(Error::Unsupported);
        }
        if !matches!(
            model,
            "flux-2-max"
                | "flux-2-pro-preview"
                | "flux-2-pro"
                | "flux-2-flex"
                | "flux-2-klein-4b"
                | "flux-2-klein-9b-preview"
                | "flux-2-klein-9b"
                | "flux-kontext-max"
                | "flux-kontext-pro"
                | "flux-pro-1.1-ultra"
                | "flux-pro-1.1"
                | "flux-pro"
                | "flux-dev"
        ) || !body.is_object()
        {
            return Err(Error::Configuration("unsupported BFL model or body"));
        }
        let v = self.send(Method::POST, model, Some(body)).await?;
        if v.get("id").and_then(Value::as_str).is_none()
            || v.get("polling_url").and_then(Value::as_str).is_none()
        {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    /// Poll the returned URL only on an official BFL host (or this client's loopback mock server).
    pub async fn bfl_status(&self, polling_url: &str) -> Result<Value, Error> {
        if self.provider != Provider::BlackForest {
            return Err(Error::Unsupported);
        }
        let url =
            Url::parse(polling_url).map_err(|_| Error::Configuration("invalid polling URL"))?;
        let host = url
            .host_str()
            .ok_or(Error::Configuration("invalid polling host"))?;
        let local = url.scheme() == "http"
            && self.base.host() == url.host()
            && self.base.port_or_known_default() == url.port_or_known_default()
            && matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback());
        if !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !(local
                || url.scheme() == "https" && (host == "bfl.ai" || host.ends_with(".bfl.ai")))
        {
            return Err(Error::Configuration("untrusted polling URL"));
        }
        self.send(Method::GET, polling_url, None).await
    }
    /// Submit an image or video generation to Luma Agents; the caller supplies model-specific options.
    pub async fn luma_submit(&self, body: Value) -> Result<Value, Error> {
        if self.provider != Provider::Luma {
            return Err(Error::Unsupported);
        }
        if !matches!(
            body.get("type").and_then(Value::as_str),
            Some("image" | "video")
        ) {
            return Err(Error::Configuration("image or video type required"));
        }
        let v = self.send(Method::POST, "generations", Some(body)).await?;
        if v.get("id").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn luma_status(&self, id: &str) -> Result<Value, Error> {
        if self.provider != Provider::Luma {
            return Err(Error::Unsupported);
        }
        if !safe_id(id) {
            return Err(Error::Configuration("invalid generation ID"));
        }
        let v = self
            .send(Method::GET, &format!("generations/{id}"), None)
            .await?;
        if v.get("state").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    /// Submit an Alibaba image or image-to-video job using a regional media endpoint.
    pub async fn alibaba_media_submit(
        &self,
        op: AlibabaMediaOperation,
        body: Value,
    ) -> Result<Value, Error> {
        if self.provider != Provider::AlibabaMedia {
            return Err(Error::Unsupported);
        }
        if !body.is_object() {
            return Err(Error::Configuration("request body must be an object"));
        }
        let path = match op {
            AlibabaMediaOperation::TextToImage => "services/aigc/text2image/image-synthesis",
            AlibabaMediaOperation::ImageToVideo => "services/aigc/image2video/video-synthesis/",
        };
        let v = self.send(Method::POST, path, Some(body)).await?;
        if v.pointer("/output/task_id")
            .and_then(Value::as_str)
            .is_none()
        {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn alibaba_media_status(&self, id: &str) -> Result<Value, Error> {
        if self.provider != Provider::AlibabaMedia {
            return Err(Error::Unsupported);
        }
        if !safe_id(id) {
            return Err(Error::Configuration("invalid task ID"));
        }
        self.send(Method::GET, &format!("tasks/{id}"), None).await
    }
    /// Generate an image using Ideogram 4.5; returns provider JSON with temporary URLs.
    pub async fn ideogram_image(&self, prompt: &str) -> Result<Value, Error> {
        if self.provider != Provider::Ideogram {
            return Err(Error::Unsupported);
        }
        if prompt.trim().is_empty() {
            return Err(Error::Configuration("prompt required"));
        }
        let v = self
            .send(
                Method::POST,
                "image/generate/ideogram-4-5",
                Some(json!({"prompt":prompt})),
            )
            .await?;
        if !v.get("data").is_some_and(Value::is_array) {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    /// Create a GPU pod using the caller's Runpod-specific configuration.
    pub async fn runpod_create_pod(&self, body: Value) -> Result<Value, Error> {
        if self.provider != Provider::Runpod {
            return Err(Error::Unsupported);
        }
        if body.get("computeType").and_then(Value::as_str) != Some("GPU") {
            return Err(Error::Configuration("GPU computeType required"));
        }
        let v = self.send(Method::POST, "pods", Some(body)).await?;
        if v.get("id").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn runpod_pods(&self) -> Result<Value, Error> {
        if self.provider != Provider::Runpod {
            return Err(Error::Unsupported);
        }
        let v = self.send(Method::GET, "pods", None).await?;
        if !v.is_array() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    /// Start, stop or restart a Pod. A returned response does not guarantee readiness.
    pub async fn runpod_pod_action(
        &self,
        id: &str,
        action: RunpodPodAction,
    ) -> Result<Value, Error> {
        if self.provider != Provider::Runpod {
            return Err(Error::Unsupported);
        }
        if !safe_id(id) {
            return Err(Error::Configuration("invalid pod ID"));
        }
        let action = match action {
            RunpodPodAction::Start => "start",
            RunpodPodAction::Stop => "stop",
            RunpodPodAction::Restart => "restart",
        };
        self.send(Method::POST, &format!("pods/{id}/{action}"), None)
            .await
    }
    /// Permanently delete a specific Pod. A 204 means the provider accepted deletion.
    pub async fn runpod_delete_pod(&self, id: &str) -> Result<(), Error> {
        if self.provider != Provider::Runpod {
            return Err(Error::Unsupported);
        }
        if !safe_id(id) {
            return Err(Error::Configuration("invalid pod ID"));
        }
        let response = self
            .request(Method::DELETE, &format!("pods/{id}"), None)
            .await?;
        if response.status() != StatusCode::NO_CONTENT {
            return Err(Error::InvalidResponse);
        }
        Ok(())
    }
    pub async fn runpod_job(&self, endpoint: &str, input: Value) -> Result<Value, Error> {
        if self.provider != Provider::RunpodServerless {
            return Err(Error::Unsupported);
        }
        if !safe_id(endpoint) || !input.is_object() {
            return Err(Error::Configuration("invalid endpoint or input"));
        }
        let v = self
            .send(
                Method::POST,
                &format!("{endpoint}/run"),
                Some(json!({"input":input})),
            )
            .await?;
        if v.get("id").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn runpod_job_status(&self, endpoint: &str, id: &str) -> Result<Value, Error> {
        if self.provider != Provider::RunpodServerless {
            return Err(Error::Unsupported);
        }
        if !safe_id(endpoint) || !safe_id(id) {
            return Err(Error::Configuration("invalid endpoint or job ID"));
        }
        self.send(Method::GET, &format!("{endpoint}/status/{id}"), None)
            .await
    }
    /// Cancel, retry, or fetch incremental outputs of a serverless job.
    pub async fn runpod_job_action(
        &self,
        endpoint: &str,
        id: &str,
        action: RunpodJobAction,
    ) -> Result<Value, Error> {
        if self.provider != Provider::RunpodServerless {
            return Err(Error::Unsupported);
        }
        if !safe_id(endpoint) || !safe_id(id) {
            return Err(Error::Configuration("invalid endpoint or job ID"));
        }
        let (method, name) = match action {
            RunpodJobAction::Cancel => (Method::POST, "cancel"),
            RunpodJobAction::Retry => (Method::POST, "retry"),
            RunpodJobAction::Stream => (Method::GET, "stream"),
        };
        self.send(method, &format!("{endpoint}/{name}/{id}"), None)
            .await
    }
    /// Submit an image or video task using the documented model-specific request body.
    pub async fn runway_submit(
        &self,
        operation: RunwayOperation,
        body: Value,
    ) -> Result<Value, Error> {
        if self.provider != Provider::Runway {
            return Err(Error::Unsupported);
        }
        if !body.is_object() {
            return Err(Error::Configuration("request body must be an object"));
        }
        let path = match operation {
            RunwayOperation::TextToImage => "text_to_image",
            RunwayOperation::TextToVideo => "text_to_video",
            RunwayOperation::ImageToVideo => "image_to_video",
        };
        let v = self.send(Method::POST, path, Some(body)).await?;
        if v.get("id").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn runway_status(&self, id: &str) -> Result<Value, Error> {
        if self.provider != Provider::Runway {
            return Err(Error::Unsupported);
        }
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
        {
            return Err(Error::Configuration("invalid task ID"));
        }
        let v = self.send(Method::GET, &format!("tasks/{id}"), None).await?;
        if v.get("status").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    /// Submit a model-specific input to fal's queue. `model_path` identifies its endpoint.
    pub async fn fal_submit(&self, model_path: &str, input: Value) -> Result<Value, Error> {
        if self.provider != Provider::Fal {
            return Err(Error::Unsupported);
        }
        if !safe_relative_path(model_path) || !input.is_object() {
            return Err(Error::Configuration("invalid fal model path or input"));
        }
        let v = self.send(Method::POST, model_path, Some(input)).await?;
        if v.get("request_id").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    /// Query a fal request using the application path supplied by the caller, not a server-provided URL.
    pub async fn fal_status(&self, application: &str, id: &str) -> Result<Value, Error> {
        self.fal_get(application, id, true).await
    }
    pub async fn fal_result(&self, application: &str, id: &str) -> Result<Value, Error> {
        self.fal_get(application, id, false).await
    }
    async fn fal_get(&self, application: &str, id: &str, status: bool) -> Result<Value, Error> {
        if self.provider != Provider::Fal {
            return Err(Error::Unsupported);
        }
        if !safe_relative_path(application)
            || id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
        {
            return Err(Error::Configuration(
                "invalid fal application or request ID",
            ));
        }
        self.send(
            Method::GET,
            &format!(
                "{application}/requests/{id}{}",
                if status { "/status" } else { "" }
            ),
            None,
        )
        .await
    }
    /// List Lambda Cloud instances, including their provider-specific fields.
    pub async fn lambda_instances(&self) -> Result<Value, Error> {
        if self.provider != Provider::LambdaCloud {
            return Err(Error::Unsupported);
        }
        let v = self.send(Method::GET, "instances", None).await?;
        if !v.get("data").is_some_and(Value::is_array) {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    /// Launch one instance. The caller selects an existing SSH key and is responsible for costs.
    pub async fn lambda_launch(
        &self,
        region: &str,
        instance_type: &str,
        ssh_key: &str,
    ) -> Result<Value, Error> {
        if self.provider != Provider::LambdaCloud {
            return Err(Error::Unsupported);
        }
        if [region, instance_type, ssh_key]
            .iter()
            .any(|s| s.trim().is_empty())
        {
            return Err(Error::Configuration(
                "region, instance type and SSH key name required",
            ));
        }
        let v = self.send(Method::POST, "instance-operations/launch", Some(json!({"region_name":region,"instance_type_name":instance_type,"ssh_key_names":[ssh_key],"file_system_names":[],"quantity":1}))).await?;
        if !v.pointer("/data/instance_ids").is_some_and(Value::is_array) {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    /// Terminate the specified instance; no retry is performed.
    pub async fn lambda_terminate(&self, id: &str) -> Result<Value, Error> {
        if self.provider != Provider::LambdaCloud {
            return Err(Error::Unsupported);
        }
        if id.trim().is_empty() {
            return Err(Error::Configuration("instance ID required"));
        }
        let v = self
            .send(
                Method::POST,
                "instance-operations/terminate",
                Some(json!({"instance_ids":[id]})),
            )
            .await?;
        if !v
            .pointer("/data/terminated_instances")
            .is_some_and(Value::is_array)
        {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn lambda_restart(&self, id: &str) -> Result<Value, Error> {
        if self.provider != Provider::LambdaCloud {
            return Err(Error::Unsupported);
        }
        if id.trim().is_empty() {
            return Err(Error::Configuration("instance ID required"));
        }
        let v = self
            .send(
                Method::POST,
                "instance-operations/restart",
                Some(json!({"instance_ids":[id]})),
            )
            .await?;
        if !v
            .pointer("/data/restarted_instances")
            .is_some_and(Value::is_array)
        {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    pub async fn digitalocean_droplets(&self) -> Result<Value, Error> {
        if self.provider != Provider::DigitalOcean {
            return Err(Error::Unsupported);
        }
        let v = self.send(Method::GET, "droplets", None).await?;
        if !v.get("droplets").is_some_and(Value::is_array) {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    /// Fetch one page of GPU Droplets. Follow the pagination metadata explicitly.
    pub async fn digitalocean_gpu_droplets(
        &self,
        page: u32,
        per_page: u16,
    ) -> Result<Value, Error> {
        if self.provider != Provider::DigitalOcean {
            return Err(Error::Unsupported);
        }
        if page == 0 || !(1..=200).contains(&per_page) {
            return Err(Error::Configuration("invalid pagination"));
        }
        let v = self
            .send(
                Method::GET,
                &format!("droplets?type=gpus&page={page}&per_page={per_page}"),
                None,
            )
            .await?;
        if !v.get("droplets").is_some_and(Value::is_array) {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
    /// Permanently delete a single Droplet. Other associated resources are not requested for deletion.
    pub async fn digitalocean_delete_droplet(&self, id: u64) -> Result<(), Error> {
        if self.provider != Provider::DigitalOcean {
            return Err(Error::Unsupported);
        }
        if id == 0 {
            return Err(Error::Configuration("invalid droplet ID"));
        }
        let response = self
            .request(Method::DELETE, &format!("droplets/{id}"), None)
            .await?;
        if response.status() != StatusCode::NO_CONTENT {
            return Err(Error::InvalidResponse);
        }
        Ok(())
    }
    /// Create an explicitly configured Droplet. The caller must verify that the size is GPU-backed and be aware of charges.
    pub async fn create_droplet(
        &self,
        name: &str,
        region: &str,
        size: &str,
        image: &str,
    ) -> Result<Value, Error> {
        if self.provider != Provider::DigitalOcean {
            return Err(Error::Unsupported);
        }
        if [name, region, size, image]
            .iter()
            .any(|v| v.trim().is_empty())
        {
            return Err(Error::Configuration(
                "name, region, GPU size and image required",
            ));
        }
        let v = self
            .send(
                Method::POST,
                "droplets",
                Some(json!({"name":name,"region":region,"size":size,"image":image})),
            )
            .await?;
        if v.pointer("/droplet/id").and_then(Value::as_u64).is_none() {
            return Err(Error::InvalidResponse);
        }
        Ok(v)
    }
}

/// One native server-sent event. The data is not logged and can include private content.
#[derive(Debug)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: Value,
}
/// Incremental AWS event-stream reader, preserving event names and JSON payloads.
pub struct BedrockStream {
    response: reqwest::Response,
    buffer: Vec<u8>,
    saw_stop: bool,
    done: bool,
}
impl BedrockStream {
    pub async fn next_event(&mut self) -> Result<Option<SseEvent>, Error> {
        if self.done {
            return Ok(None);
        }
        loop {
            if self.buffer.len() >= 12 {
                let len = u32::from_be_bytes(self.buffer[0..4].try_into().unwrap()) as usize;
                let headers_len =
                    u32::from_be_bytes(self.buffer[4..8].try_into().unwrap()) as usize;
                let prelude_crc = u32::from_be_bytes(self.buffer[8..12].try_into().unwrap());
                if !(16..=16 * 1024 * 1024).contains(&len)
                    || headers_len > len - 16
                    || crc32fast::hash(&self.buffer[..8]) != prelude_crc
                {
                    return Err(Error::InvalidResponse);
                }
                if self.buffer.len() >= len {
                    let frame: Vec<u8> = self.buffer.drain(..len).collect();
                    let crc = u32::from_be_bytes(frame[len - 4..].try_into().unwrap());
                    if crc32fast::hash(&frame[..len - 4]) != crc {
                        return Err(Error::InvalidResponse);
                    }
                    let headers = &frame[12..12 + headers_len];
                    let (mut pos, mut kind, mut name) = (0, None, None);
                    while pos < headers.len() {
                        let name_len = headers[pos] as usize;
                        pos += 1;
                        if pos + name_len + 3 > headers.len() {
                            return Err(Error::InvalidResponse);
                        }
                        let key = std::str::from_utf8(&headers[pos..pos + name_len])
                            .map_err(|_| Error::InvalidResponse)?;
                        pos += name_len;
                        if headers[pos] != 7 {
                            return Err(Error::InvalidResponse);
                        }
                        pos += 1;
                        let value_len =
                            u16::from_be_bytes([headers[pos], headers[pos + 1]]) as usize;
                        pos += 2;
                        if pos + value_len > headers.len() {
                            return Err(Error::InvalidResponse);
                        }
                        let value = std::str::from_utf8(&headers[pos..pos + value_len])
                            .map_err(|_| Error::InvalidResponse)?;
                        pos += value_len;
                        if key == ":message-type" {
                            kind = Some(value.to_owned());
                        }
                        if key == ":event-type" {
                            name = Some(value.to_owned());
                        }
                    }
                    if kind.as_deref() != Some("event") {
                        return Err(Error::InvalidResponse);
                    }
                    let name = name.ok_or(Error::InvalidResponse)?;
                    let data: Value = serde_json::from_slice(&frame[12 + headers_len..len - 4])
                        .map_err(|_| Error::InvalidResponse)?;
                    if name == "messageStop" {
                        self.saw_stop = true;
                    }
                    return Ok(Some(SseEvent {
                        event: Some(name),
                        data,
                    }));
                }
            }
            match self.response.chunk().await.map_err(|_| Error::Transport)? {
                Some(chunk) => {
                    self.buffer.extend_from_slice(&chunk);
                    if self.buffer.len() > 16 * 1024 * 1024 {
                        return Err(Error::InvalidResponse);
                    }
                }
                None if self.buffer.is_empty() && self.saw_stop => {
                    self.done = true;
                    return Ok(None);
                }
                None => return Err(Error::InvalidResponse),
            }
        }
    }
}

/// Incremental, bounded SSE reader. Dropping it closes the response without retries.
pub struct SseStream {
    response: reqwest::Response,
    buffer: Vec<u8>,
    done: bool,
    saw_terminal: bool,
    provider: Provider,
}
impl SseStream {
    pub async fn next_event(&mut self) -> Result<Option<SseEvent>, Error> {
        if self.done {
            return Ok(None);
        }
        loop {
            // SSE lines may use LF or CRLF and HTTP chunks may split UTF-8 or delimiters.
            let lf = self
                .buffer
                .windows(2)
                .position(|w| w == b"\n\n")
                .map(|i| (i, 2));
            let crlf = self
                .buffer
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|i| (i, 4));
            let end = match (lf, crlf) {
                (Some(a), Some(b)) => Some(if a.0 < b.0 { a } else { b }),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            };
            if let Some((i, n)) = end {
                let bytes: Vec<u8> = self.buffer.drain(..i + n).collect();
                let block = std::str::from_utf8(&bytes[..i]).map_err(|_| Error::InvalidResponse)?;
                let mut event = None;
                let mut data = Vec::new();
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        event = Some(v.trim_start().to_owned());
                    }
                    if let Some(v) = line.strip_prefix("data:") {
                        data.push(v.strip_prefix(' ').unwrap_or(v));
                    }
                }
                if data.is_empty() {
                    continue;
                }
                let data = data.join("\n");
                if data == "[DONE]" {
                    self.done = true;
                    return Ok(None);
                }
                let value: Value =
                    if self.provider == Provider::Replicate && event.as_deref() == Some("output") {
                        Value::String(data)
                    } else {
                        serde_json::from_str(&data).map_err(|_| Error::InvalidResponse)?
                    };
                if self.provider == Provider::Replicate && event.as_deref() == Some("done") {
                    self.done = true;
                }
                if event.as_deref() == Some("error") || value.get("error").is_some() {
                    return Err(Error::InvalidResponse);
                }
                if matches!(self.provider, Provider::GoogleAi | Provider::Vertex)
                    && (value.pointer("/candidates/0/finishReason").is_some()
                        || value.pointer("/promptFeedback/blockReason").is_some())
                {
                    self.saw_terminal = true;
                }
                if self.provider == Provider::Anthropic && event.as_deref() == Some("message_stop")
                {
                    self.done = true;
                }
                if self.provider == Provider::Cohere
                    && (event.as_deref() == Some("message-end")
                        || value.get("type").and_then(Value::as_str) == Some("message-end"))
                {
                    self.done = true;
                }
                if matches!(self.provider, Provider::PerplexityAgent | Provider::Meta)
                    && (matches!(
                        event.as_deref(),
                        Some(
                            "response.completed"
                                | "response.failed"
                                | "response.incomplete"
                                | "response.cancelled"
                        )
                    ) || matches!(
                        value.get("type").and_then(Value::as_str),
                        Some(
                            "response.completed"
                                | "response.failed"
                                | "response.incomplete"
                                | "response.cancelled"
                        )
                    ))
                {
                    self.done = true;
                }
                return Ok(Some(SseEvent { event, data: value }));
            }
            if self.buffer.len() > 1024 * 1024 {
                return Err(Error::InvalidResponse);
            }
            match self.response.chunk().await.map_err(|_| Error::Transport)? {
                Some(chunk) => {
                    self.buffer.extend_from_slice(&chunk);
                    if self.buffer.len() > 1024 * 1024 {
                        return Err(Error::InvalidResponse);
                    }
                }
                None if matches!(self.provider, Provider::GoogleAi | Provider::Vertex)
                    && self.buffer.is_empty()
                    && self.saw_terminal =>
                {
                    self.done = true;
                    return Ok(None);
                }
                None => return Err(Error::InvalidResponse),
            }
        }
    }
}

fn safe_relative_path(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
}

#[derive(Clone, Copy, Debug)]
pub enum RunwayOperation {
    TextToImage,
    TextToVideo,
    ImageToVideo,
}

fn safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
}

#[derive(Clone, Copy, Debug)]
pub enum AlibabaMediaOperation {
    TextToImage,
    ImageToVideo,
}

fn safe_model(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

#[derive(Clone, Copy, Debug)]
pub enum KlingOperation {
    Image,
    TextToVideo,
}
impl KlingOperation {
    fn path(self) -> &'static str {
        match self {
            Self::Image => "images/generations",
            Self::TextToVideo => "videos/text2video",
        }
    }
}
