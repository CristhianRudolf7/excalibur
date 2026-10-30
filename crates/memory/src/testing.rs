//! Deterministic local doubles for the model-backed roles. They make no network calls and are
//! meant for tests and offline hosts; they are not semantic models.
use crate::{
    AdmissionClassifier, BoxFuture, Classification, Compaction, ContextWriter, EmbeddingProvider,
    ExtractedPage, ExtractionOutput, ExtractionRequest, Extractor, LocalEmbeddingModel,
    LocalModelLoader, Memory, NoteCompactor, ProviderLocation, QueryRewriter, Reranker,
    TimedSegment, Transcriber, VisionExtractor, identity::Fragment, text::words,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

/// Signed feature hashing of folded words; identical texts give identical vectors and
/// texts sharing words are close. `set_available(false)` simulates an outage.
pub struct HashEmbedder {
    model: String,
    dimensions: u32,
    available: AtomicBool,
    overrides: Mutex<BTreeMap<String, Vec<f32>>>,
    pub calls: AtomicUsize,
}
impl HashEmbedder {
    pub fn new(model: &str, dimensions: u32) -> Self {
        Self {
            model: model.into(),
            dimensions,
            available: AtomicBool::new(true),
            overrides: Mutex::default(),
            calls: AtomicUsize::new(0),
        }
    }
    pub fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::SeqCst);
    }
    /// Fix the vector of an exact text (for controlled similarity bands).
    pub fn set_vector(&self, text: &str, vector: Vec<f32>) {
        self.overrides.lock().unwrap().insert(text.into(), vector);
    }
    pub fn vector(&self, text: &str) -> Vec<f32> {
        if let Some(v) = self.overrides.lock().unwrap().get(text) {
            let mut v = v.clone();
            v.resize(self.dimensions as usize, 0.0);
            return v;
        }
        let mut out = vec![0f32; self.dimensions as usize];
        for word in words(text) {
            let h = crate::digest(format!("{}\0{word}", self.model).as_bytes());
            let bucket = usize::from_str_radix(&h[..8], 16).unwrap() % out.len();
            let sign = if u8::from_str_radix(&h[8..10], 16).unwrap().is_multiple_of(2) {
                1.0
            } else {
                -1.0
            };
            out[bucket] += sign;
        }
        out
    }
}
impl EmbeddingProvider for HashEmbedder {
    fn model(&self) -> String {
        self.model.clone()
    }
    fn source_dimensions(&self) -> u32 {
        self.dimensions
    }
    fn location(&self) -> ProviderLocation {
        ProviderLocation::Local
    }
    fn embed<'a>(
        &'a self,
        texts: &'a [String],
    ) -> BoxFuture<'a, std::result::Result<Vec<Vec<f32>>, String>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.available.load(Ordering::SeqCst) {
                return Err("embedding provider unavailable".into());
            }
            Ok(texts.iter().map(|t| self.vector(t)).collect())
        })
    }
}

pub struct FixedClassifier {
    pub name: String,
    pub result: Classification,
    pub calls: AtomicUsize,
}
impl FixedClassifier {
    pub fn new(name: &str, result: Classification) -> Self {
        Self {
            name: name.into(),
            result,
            calls: AtomicUsize::new(0),
        }
    }
}
impl AdmissionClassifier for FixedClassifier {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn classify<'a>(
        &'a self,
        _: &'a Memory,
        _: &'a Memory,
        _: f32,
    ) -> BoxFuture<'a, std::result::Result<Classification, String>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(self.result) })
    }
}

type ExtractFn = dyn Fn(&ExtractionRequest) -> ExtractionOutput + Send + Sync;
/// Extractor driven by a closure; every request is recorded for inspection.
pub struct ScriptedExtractor {
    script: Box<ExtractFn>,
    pub requests: Mutex<Vec<ExtractionRequest>>,
}
impl ScriptedExtractor {
    pub fn new(
        script: impl Fn(&ExtractionRequest) -> ExtractionOutput + Send + Sync + 'static,
    ) -> Self {
        Self {
            script: Box::new(script),
            requests: Mutex::default(),
        }
    }
}
impl Extractor for ScriptedExtractor {
    fn extract<'a>(
        &'a self,
        request: &'a ExtractionRequest,
    ) -> BoxFuture<'a, std::result::Result<ExtractionOutput, String>> {
        self.requests.lock().unwrap().push(request.clone());
        let out = (self.script)(request);
        Box::pin(async move { Ok(out) })
    }
}

/// Keeps the first `keep` entries and compiles the rest into one line.
pub struct KeepFirstCompactor {
    pub keep: usize,
}
impl NoteCompactor for KeepFirstCompactor {
    fn compact<'a>(
        &'a self,
        _: &'a str,
        entries: &'a [Fragment],
    ) -> BoxFuture<'a, std::result::Result<Compaction, String>> {
        Box::pin(async move {
            let keep = entries
                .iter()
                .take(self.keep)
                .map(|f| f.id.clone())
                .collect();
            let compiled = entries
                .iter()
                .skip(self.keep)
                .map(|f| f.text.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            Ok(Compaction {
                compiled,
                keep,
                tokens_used: 10,
            })
        })
    }
}

pub struct ReverseReranker;
impl Reranker for ReverseReranker {
    fn rerank<'a>(
        &'a self,
        _: &'a str,
        texts: &'a [String],
    ) -> BoxFuture<'a, std::result::Result<Vec<usize>, String>> {
        Box::pin(async move { Ok((0..texts.len()).rev().collect()) })
    }
}

pub struct FixedRewriter {
    pub rewrite: String,
    pub calls: AtomicUsize,
}
impl QueryRewriter for FixedRewriter {
    fn rewrite<'a>(&'a self, _: &'a str) -> BoxFuture<'a, std::result::Result<String, String>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(self.rewrite.clone()) })
    }
}

/// Returns preset pages for any document (stands in for `vision_extract`).
pub struct FixedVision(pub Vec<ExtractedPage>);
impl VisionExtractor for FixedVision {
    fn extract<'a>(
        &'a self,
        _: &'a str,
        _: &'a [u8],
    ) -> BoxFuture<'a, std::result::Result<Vec<ExtractedPage>, String>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
}
pub struct FixedTranscriber(pub Vec<TimedSegment>);
impl Transcriber for FixedTranscriber {
    fn transcribe<'a>(
        &'a self,
        _: &'a str,
        _: &'a [u8],
    ) -> BoxFuture<'a, std::result::Result<Vec<TimedSegment>, String>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
}
pub struct TitleContextWriter {
    pub calls: AtomicUsize,
}
impl ContextWriter for TitleContextWriter {
    fn context<'a>(
        &'a self,
        title: &'a str,
        section: Option<&'a str>,
        _: &'a str,
    ) -> BoxFuture<'a, std::result::Result<String, String>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok(format!(
                "From {title}{}.",
                section
                    .map(|s| format!(", section {s}"))
                    .unwrap_or_default()
            ))
        })
    }
}

/// Local loader that can be made to fail (for example, insufficient RAM).
pub struct MockLoader {
    pub inner: Arc<HashEmbedder>,
    pub fail: AtomicBool,
    pub loads: AtomicUsize,
}
struct Loaded(Arc<HashEmbedder>);
impl LocalEmbeddingModel for Loaded {
    fn embed(&self, texts: &[String]) -> std::result::Result<Vec<Vec<f32>>, String> {
        Ok(texts.iter().map(|t| self.0.vector(t)).collect())
    }
}
impl LocalModelLoader for MockLoader {
    fn model(&self) -> String {
        self.inner.model()
    }
    fn source_dimensions(&self) -> u32 {
        self.inner.source_dimensions()
    }
    fn load(&self) -> std::result::Result<Arc<dyn LocalEmbeddingModel>, String> {
        if self.fail.load(Ordering::SeqCst) {
            return Err("insufficient memory to load the local model".into());
        }
        self.loads.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(Loaded(self.inner.clone())))
    }
}
