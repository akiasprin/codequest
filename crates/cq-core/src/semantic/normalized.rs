//! Normalization emits owned records into a storage-neutral bounded sink.
use crate::{Result, model::*};
use std::collections::BTreeMap;
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "family", content = "value")]
pub enum Record {
    Entity(Entity),
    Body(Body),
    Occurrence(Occurrence),
    Relation(Relation),
    Document(DocumentBlock),
}
impl Record {
    pub fn family(&self) -> &'static str {
        match self {
            Self::Entity(_) => "entity",
            Self::Body(_) => "body",
            Self::Occurrence(_) => "occurrence",
            Self::Relation(_) => "relation",
            Self::Document(_) => "document",
        }
    }
    pub fn id(&self) -> &str {
        match self {
            Self::Entity(v) => &v.id,
            Self::Body(v) => &v.id,
            Self::Occurrence(v) => &v.id,
            Self::Relation(v) => &v.id,
            Self::Document(v) => &v.id,
        }
    }
    pub fn value(&self) -> Result<serde_json::Value> {
        Ok(match self {
            Self::Entity(v) => serde_json::to_value(v)?,
            Self::Body(v) => serde_json::to_value(v)?,
            Self::Occurrence(v) => serde_json::to_value(v)?,
            Self::Relation(v) => serde_json::to_value(v)?,
            Self::Document(v) => serde_json::to_value(v)?,
        })
    }
}
pub trait Sink {
    fn push(&mut self, unit: &str, shard: &str, record: Record) -> Result<()>;
    fn batch(&mut self, records: Vec<(String, String, Record)>) -> Result<()> {
        for (unit, shard, record) in records {
            self.push(&unit, &shard, record)?;
        }
        Ok(())
    }
}
pub struct Projection {
    pub universe: String,
    pub provenance: BTreeMap<ProvenanceId, Provenance>,
    pub sources: BTreeMap<String, Option<FileRevision>>,
}
