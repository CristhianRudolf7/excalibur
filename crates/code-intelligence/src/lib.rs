//! Shared incremental source graph and explicitly confined local LSP sessions.
mod enrichment;
pub mod graph;
pub use enrichment::*;
pub mod lsp;
pub use graph::*;
pub use lsp::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("code graph storage: {0}")]
    Store(#[from] excalibur_store::Error),
    #[error("code intelligence JSON serialization failed")]
    Json(#[from] serde_json::Error),
    #[error("local language server I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported source language")]
    UnsupportedLanguage,
    #[error("language server operation is not supported")]
    UnsupportedOperation,
    #[error("local language server request timed out")]
    Timeout,
    #[error("local language server closed")]
    Closed,
    #[error("language server protocol violation: {0}")]
    Protocol(&'static str),
    #[error("{0}")]
    Invalid(String),
}
pub type Result<T> = std::result::Result<T, Error>;
pub(crate) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub(crate) fn invalid(s: impl Into<String>) -> Error {
    Error::Invalid(s.into())
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Json,
    Markdown,
}
impl Language {
    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "json" => Some(Self::Json),
            "md" | "markdown" => Some(Self::Markdown),
            _ => None,
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Tsx => "typescriptreact",
            Self::Json => "json",
            Self::Markdown => "markdown",
        }
    }
    pub(crate) fn grammar(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::Json => tree_sitter_json::LANGUAGE.into(),
            Self::Markdown => tree_sitter_md::LANGUAGE.into(),
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyntaxIssue {
    pub kind: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub range: Range,
    pub message: String,
}

impl From<excalibur_store::rusqlite::Error> for Error {
    fn from(error: excalibur_store::rusqlite::Error) -> Self {
        Self::Store(error.into())
    }
}
