//! Cheap token estimation.
//!
//! When a provider reports real usage we use that. For measuring *context size
//! before* a call we fall back to a heuristic: CJK characters are roughly one
//! token each in most tokenizers, latin text is roughly one token per 4 chars.

pub fn estimate(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        let c = ch as u32;
        if c >= 0x2E80 && c <= 0x9FFF
            || c >= 0xAC00 && c <= 0xD7AF // 韩文音节 Hangul syllables
            || c >= 0xF900 && c <= 0xFAFF
            || c >= 0xFF00 && c <= 0xFFEF
            || c >= 0x20000 && c <= 0x2A6DF // CJK 扩展 B
        {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    // 1 token per CJK char, ~4 latin chars per token.
    cjk + (other + 3) / 4
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
    }

    pub fn total(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_read_input_tokens
            + self.cache_creation_input_tokens
    }

    /// Fraction of input tokens that were served from cache, 0..1.
    pub fn cache_hit_rate(&self) -> f64 {
        let denom = self.input_tokens
            + self.cache_read_input_tokens
            + self.cache_creation_input_tokens;
        if denom == 0 {
            0.0
        } else {
            self.cache_read_input_tokens as f64 / denom as f64
        }
    }
}
