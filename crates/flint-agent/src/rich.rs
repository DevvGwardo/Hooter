//! Design-contract declarations only; execution is unsupported in B1.
use crate::orchestration::*;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AssetId(pub String);

impl AssetId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AssetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AssetRef {
    pub id: AssetId,
    pub mime_type: String,
    pub bytes: u64,
    pub digest: Digest,
}

impl AssetRef {
    pub fn new(id: AssetId, mime_type: impl Into<String>, bytes: u64, digest: Digest) -> Self {
        Self {
            id,
            mime_type: mime_type.into(),
            bytes,
            digest,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ContentRef {
    Inline(String),  // <=16 KiB, safe plain/Markdown source.
    Asset(AssetRef), // Engine-owned relative object, not arbitrary PathBuf.
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DiagramLanguage {
    Mermaid,
    Dot,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RichPart {
    Text {
        content: ContentRef,
        markdown: bool,
    },
    Image {
        asset: AssetRef,
        alt: String,
    },
    HtmlSource {
        content: ContentRef,
    },
    DiagramSource {
        language: DiagramLanguage,
        content: ContentRef,
    },
    ResourceLink {
        uri: String,
        name: String,
        mime_type: Option<String>,
    },
    Unsupported {
        mime_type: String,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RichOutput {
    pub row: RowId,
    pub call_id: Option<String>,
    pub parts: Vec<RichPart>, // <=32.
    pub model_text: String,   // Existing bounded text result + nontext placeholders.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RichLimits {
    pub parts: usize,               // 32.
    pub decoded_asset_bytes: usize, // <=1 MiB at current MCP 1 MiB frame cap.
    pub output_bytes: usize,        // <=4 MiB across ACP output parts.
    pub source_bytes: usize,        // <=256 KiB HTML/diagram.
}
