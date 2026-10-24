//! Font data shared by the native and browser text backends.
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleFontAttachment {
    pub name: String,
    pub mime_type: Option<String>,
    pub families: Vec<String>,
    pub data: Arc<[u8]>,
}

pub const MAX_MEMORY_SUBTITLE_FONT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_MEMORY_SUBTITLE_FONT_TOTAL_BYTES: usize = 128 * 1024 * 1024;

impl SubtitleFontAttachment {
    pub fn new(
        name: impl Into<String>,
        mime_type: Option<String>,
        families: Vec<String>,
        data: impl Into<Arc<[u8]>>,
    ) -> Self {
        Self {
            name: name.into(),
            mime_type,
            families,
            data: data.into(),
        }
    }

    pub fn byte_len(&self) -> usize {
        self.data.len()
    }
}
