use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct LspSymbol {
    pub name: String,
    pub kind: String,
    pub detail: Option<String>,
    pub range: LspRange,
    pub selection_range: LspRange,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LspLocation {
    pub uri: String,
    pub range: LspRange,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}
