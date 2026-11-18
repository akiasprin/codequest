use super::embeddings::TokenVectors;
use crate::model::*;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, OnceLock};
pub fn tokenize(text: &str) -> Vec<String> {
    let mut expanded = String::new();
    let mut previous = false;
    for c in text.chars() {
        if c.is_uppercase() && previous {
            expanded.push(' ');
        }
        if c.is_alphanumeric() {
            expanded.extend(c.to_lowercase());
        } else {
            expanded.push(' ');
        }
        previous = c.is_lowercase();
    }
    expanded.split_whitespace().map(str::to_owned).collect()
}
pub fn terms(entity: &Entity) -> Vec<String> {
    tokenize(&format!(
        "{} {} {} {}",
        entity.name,
        entity.qualified_name,
        entity.signature,
        entity
            .attributes
            .get("search_text")
            .map(String::as_str)
            .unwrap_or("")
    ))
}
pub struct SearchIndex {
    documents: Vec<Vec<String>>,
    df: HashMap<String, usize>,
    average: f64,
    vectors: Vec<OnceLock<Vec<f32>>>,
    model: Option<Arc<TokenVectors>>,
}
impl SearchIndex {
    pub fn new(entities: &[Entity]) -> Self {
        Self::with_model(entities, None)
    }
    pub fn with_model(entities: &[Entity], model: Option<Arc<TokenVectors>>) -> Self {
        let documents: Vec<_> = entities.iter().map(terms).collect();
        let mut df = HashMap::new();
        for d in &documents {
            for term in d.iter().collect::<BTreeSet<_>>() {
                *df.entry(term.clone()).or_default() += 1;
            }
        }
        let average =
            documents.iter().map(Vec::len).sum::<usize>() as f64 / documents.len().max(1) as f64;
        let vectors = (0..documents.len()).map(|_| OnceLock::new()).collect();
        Self {
            documents,
            df,
            average,
            vectors,
            model,
        }
    }
    pub fn bm25(&self, index: usize, query: &str) -> f64 {
        self.bm25_terms(index, &tokenize(query))
    }
    pub fn bm25_terms(&self, index: usize, terms: &[String]) -> f64 {
        let doc = &self.documents[index];
        let mut score = 0.;
        for term in terms {
            let freq = doc.iter().filter(|t| *t == term).count() as f64;
            if freq == 0. {
                continue;
            }
            let df = *self.df.get(term).unwrap_or(&0) as f64;
            let idf = ((self.documents.len() as f64 - df + 0.5) / (df + 0.5) + 1.).ln();
            score += idf * freq * 2.2
                / (freq + 1.2 * (0.25 + 0.75 * doc.len() as f64 / self.average.max(1.)));
        }
        score
    }
    pub fn model_name(&self) -> String {
        self.model.as_ref().map(|m|format!("nomic-embed-code static token vectors; sha256={}; dimension={}; first 4096 document terms; not sentence-transformer inference",m.digest,m.dimension)).unwrap_or_else(||"offline sparse random indexing; no pretrained model".into())
    }
    pub fn prepare_vector(&self, terms: &[String]) -> Vec<f32> {
        self.model
            .as_ref()
            .map(|m| m.embed(terms))
            .unwrap_or_else(|| embedding(terms))
    }
    pub fn semantic_vector(&self, index: usize, query: &[f32]) -> f64 {
        self.vectors[index]
            .get_or_init(|| {
                let terms = &self.documents[index];
                self.prepare_vector(&terms[..terms.len().min(4096)])
            })
            .iter()
            .zip(query)
            .map(|(a, b)| (*a * *b) as f64)
            .sum::<f64>()
            .max(0.)
    }
    pub fn semantic(&self, index: usize, query: &str) -> f64 {
        self.semantic_vector(index, &self.prepare_vector(&tokenize(query)))
    }
}
/// Deterministic sparse random indexing is the offline fallback. It is named
/// explicitly, never presented as a neural model or a binding confidence.
pub fn embedding(terms: &[String]) -> Vec<f32> {
    let mut vector = vec![0f32; 384];
    for term in terms {
        let canonical = match term.as_str() {
            "emit" | "write" | "print" | "serialize" => "output",
            "read" | "parse" | "decode" | "deserialize" => "input",
            "failure" | "fail" | "error" => "error",
            s => s,
        };
        let hash = super::super::model::digest(canonical.as_bytes());
        for i in 0..8 {
            let x = u32::from_str_radix(&hash[i * 8..i * 8 + 8], 16).unwrap();
            vector[(x as usize >> 1) % 384] += if x & 1 == 0 { 1. } else { -1. };
        }
    }
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt().max(1.);
    for v in &mut vector {
        *v /= norm;
    }
    vector
}
