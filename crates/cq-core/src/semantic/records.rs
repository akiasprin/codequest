//! Record-level analysis transport. Global algorithms retain identity/range
//! indexes, while complete fact payloads belong to the storage-owned journal.
pub use super::normalized::Record;
use crate::{Result, model::*};
pub trait Source: Send + Sync {
    fn visit(&self, visitor: &mut dyn FnMut(Record) -> Result<()>) -> Result<()>;
    fn get(&self, family: &str, id: &str) -> Result<Option<Record>>;
}
pub trait Store: Source {
    fn put(&self, header: &SemanticSnapshot, record: Record, replace: bool) -> Result<()>;
}
pub fn drain(s: &mut SemanticSnapshot, store: &dyn Store) -> Result<()> {
    for r in std::mem::take(&mut s.entities) {
        store.put(s, Record::Entity(r), false)?;
    }
    for r in std::mem::take(&mut s.bodies) {
        store.put(s, Record::Body(r), false)?;
    }
    for r in std::mem::take(&mut s.occurrences) {
        store.put(s, Record::Occurrence(r), false)?;
    }
    for r in std::mem::take(&mut s.relations) {
        store.put(s, Record::Relation(r), false)?;
    }
    for r in std::mem::take(&mut s.documents) {
        store.put(s, Record::Document(r), false)?;
    }
    Ok(())
}
