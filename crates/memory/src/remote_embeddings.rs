//! Remote embedding adapter over the shared provider client. The default remote profile is
//! OpenAI `text-embedding-3-small`, which supports shortened (Matryoshka) vectors, so the
//! request asks for the stored dimension directly.
use crate::{BoxFuture, EmbeddingProvider, ProviderLocation};
use excalibur_providers::{Provider, ProviderClient, Secret};

pub const OPENAI_SMALL_MODEL: &str = "text-embedding-3-small";
/// Maximum inputs per request sent by this adapter.
pub const REMOTE_BATCH: usize = 256;

pub struct RemoteEmbedder {
    client: ProviderClient,
    model: String,
    dimensions: Option<u32>,
    source_dimensions: u32,
}
impl std::fmt::Debug for RemoteEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteEmbedder")
            .field("model", &self.model)
            .field("dimensions", &self.dimensions)
            .finish_non_exhaustive()
    }
}
impl RemoteEmbedder {
    /// OpenAI `text-embedding-3-small` asked for 512-dimensional vectors.
    pub fn openai_small(api_key: Secret) -> Result<Self, String> {
        let client =
            ProviderClient::new(Provider::OpenAi, api_key).map_err(|error| error.to_string())?;
        Ok(Self::with_client(
            client,
            OPENAI_SMALL_MODEL,
            Some(512),
            512,
        ))
    }
    /// Any OpenAI-compatible client (tests use a loopback mock base URL).
    pub fn with_client(
        client: ProviderClient,
        model: impl Into<String>,
        dimensions: Option<u32>,
        source_dimensions: u32,
    ) -> Self {
        Self {
            client,
            model: model.into(),
            dimensions,
            source_dimensions,
        }
    }
}
impl EmbeddingProvider for RemoteEmbedder {
    fn model(&self) -> String {
        match self.dimensions {
            Some(d) => format!("{}@{d}", self.model),
            None => self.model.clone(),
        }
    }
    fn source_dimensions(&self) -> u32 {
        self.source_dimensions
    }
    fn location(&self) -> ProviderLocation {
        ProviderLocation::Remote
    }
    fn embed<'a>(
        &'a self,
        texts: &'a [String],
    ) -> BoxFuture<'a, std::result::Result<Vec<Vec<f32>>, String>> {
        Box::pin(async move {
            let mut out = Vec::with_capacity(texts.len());
            for batch in texts.chunks(REMOTE_BATCH) {
                let vectors = self
                    .client
                    .embeddings(&self.model, batch, self.dimensions)
                    .await
                    .map_err(|error| format!("embedding provider unavailable: {error}"))?;
                if vectors
                    .iter()
                    .any(|v| v.len() != self.source_dimensions as usize)
                {
                    return Err("embedding provider returned unexpected dimensions".into());
                }
                out.extend(vectors);
            }
            Ok(out)
        })
    }
}
