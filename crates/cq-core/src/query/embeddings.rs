//! Frozen static token embeddings. This is not sentence transformer inference.
use crate::{Error, Result, model::digest};
use std::collections::HashMap;
/// Vector retrieval is experimental and excluded from the default index.
pub fn enabled() -> bool {
    std::env::var("CQ_EXPERIMENTAL_VECTOR_SEARCH").is_ok_and(|v| v == "1")
}
pub struct TokenVectors {
    tokens: HashMap<String, usize>,
    bytes: Vec<u8>,
    pub dimension: usize,
    pub digest: String,
}
impl TokenVectors {
    fn configured_directory() -> Option<std::path::PathBuf> {
        std::env::var_os("CQ_TOKEN_VECTORS")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::current_exe().ok().and_then(|e| {
                    Some(
                        e.parent()?
                            .parent()?
                            .parent()?
                            .join(".codequest-tools/embeddings"),
                    )
                })
            })
    }
    pub fn configured_identity() -> Result<serde_json::Value> {
        if !enabled() {
            return Ok(serde_json::json!({"label":"disabled","enabled":false}));
        }
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let Some(dir) =
            Self::configured_directory().filter(|d| d.join("code_vectors.bin").is_file())
        else {
            return Ok(
                serde_json::json!({"label":"offline sparse random indexing; no pretrained model"}),
            );
        };
        let mut file = std::fs::File::open(dir.join("code_vectors.bin"))?;
        let mut header = [0; 8];
        file.read_exact(&mut header)?;
        let dimension = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let mut hash = Sha256::new();
        hash.update(header);
        std::io::copy(&mut file, &mut hash)?;
        let vector = format!("{:x}", hash.finalize());
        let vocabulary = crate::model::digest(&std::fs::read(dir.join("code_tokens.txt"))?);
        Ok(
            serde_json::json!({"label":format!("nomic-embed-code static token vectors; sha256={vector}; dimension={dimension}; first 4096 document terms; not sentence-transformer inference"),"vectors":vector,"vocabulary":vocabulary,"dimension":dimension}),
        )
    }
    pub fn configured() -> Result<Option<std::sync::Arc<Self>>> {
        if !enabled() {
            return Ok(None);
        }
        let dir = Self::configured_directory();
        dir.filter(|d| d.join("code_vectors.bin").is_file())
            .map(|d| {
                Self::load(
                    &std::fs::read_to_string(d.join("code_tokens.txt"))?,
                    std::fs::read(d.join("code_vectors.bin"))?,
                )
                .map(std::sync::Arc::new)
            })
            .transpose()
    }
    pub fn load(vocabulary: &str, bytes: Vec<u8>) -> Result<Self> {
        if bytes.len() < 8 {
            return Err(Error::new("InvalidEmbeddingModel", "missing header"));
        }
        let count = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let dimension = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        if dimension == 0 || dimension > 4096 || bytes.len() != 8 + count.saturating_mul(dimension)
        {
            return Err(Error::new("InvalidEmbeddingModel", "invalid dimensions"));
        }
        let tokens: HashMap<_, _> = vocabulary
            .lines()
            .enumerate()
            .map(|(i, t)| (t.trim().to_owned(), i))
            .collect();
        if vocabulary.lines().count() != count {
            return Err(Error::new(
                "InvalidEmbeddingModel",
                "vocabulary size mismatch",
            ));
        }
        let digest = digest(&bytes);
        Ok(Self {
            tokens,
            bytes,
            dimension,
            digest,
        })
    }
    pub fn embed(&self, terms: &[String]) -> Vec<f32> {
        let mut v = vec![0.; self.dimension];
        for term in terms {
            if let Some(i) = self.tokens.get(term) {
                let start = 8 + i * self.dimension;
                for (value, byte) in v.iter_mut().zip(&self.bytes[start..start + self.dimension]) {
                    *value += *byte as i8 as f32 / 127.;
                }
            } else {
                let h = digest(term.as_bytes());
                for at in 0..8 {
                    let n = u32::from_str_radix(&h[at * 8..at * 8 + 8], 16).unwrap();
                    v[(n as usize >> 1) % self.dimension] += if n & 1 == 0 { 1. } else { -1. };
                }
            }
        }
        let norm = v.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
        v.iter_mut().for_each(|v| *v /= norm);
        v
    }
}
