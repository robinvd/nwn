use std::io::Read;

use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::TextEdit;

/// An object representing a single (stack) frame
#[derive(Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
pub struct Frame {
    pub file: String,
    pub line: u64,
}

/// The message type that gets send from a runtime.
#[derive(Debug, Deserialize, Serialize)]
pub struct RawEntry {
    pub frames: Vec<Frame>,
    #[serde(flatten)]
    pub inner: RawEntryInner,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum RawEntryInner {
    Simple {
        out: String,
        // insert: Option<String>,
        kind: Option<RawEntryKind>,
    },
    Edits {
        changes: Vec<TextEdit>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RawEntryKind {
    Output,
    Debug,
    Error,
    Regex,
}

impl RawEntry {
    pub fn from_json(reader: impl Read) -> Result<Self, Box<dyn std::error::Error>> {
        let entry = serde_json::de::from_reader(reader);
        match entry {
            Ok(entry) => Ok(entry),
            Err(err) => {
                log::error!("loading err {:?}", err);
                Err(Box::new(err))
            }
        }
    }
}
