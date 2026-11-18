//! Frozen static token embeddings. This is not sentence transformer inference.
use crate::{Error, Result, model::digest};
use std::collections::HashMap;
pub struct TokenVectors {
    tokens: HashMap<String, usize>,
    bytes: Vec<u8>,
    pub dimension: usize,
    pub digest: String,
}
impl TokenVectors {
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
