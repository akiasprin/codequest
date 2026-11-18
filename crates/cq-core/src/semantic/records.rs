//! Record-level analysis transport. Global algorithms retain identity/range
//! indexes, while complete fact payloads belong to the storage-owned journal.
pub use super::normalized::Record;
use crate::{Result, model::*};
pub trait Source: Send + Sync {
    fn visit(&self, visitor: &mut dyn FnMut(Record) -> Result<()>) -> Result<()>;
    fn get(&self, family: &str, id: &str) -> Result<Option<Record>>;
    fn visit_family(
        &self,
        family: &str,
        visitor: &mut dyn FnMut(Record) -> Result<()>,
    ) -> Result<()> {
        self.visit(&mut |r| {
            if r.family() == family {
                visitor(r)
            } else {
                Ok(())
            }
        })
    }
    fn family_len(&self, _family: &str) -> Result<Option<u64>> {
        Ok(None)
    }
}
pub trait Store: Source {
    fn task_progress(
        &self,
        _project: &str,
        _phase: &str,
        _label: &str,
        _done: u64,
        _total: Option<u64>,
        _unit: &str,
    ) {
    }
    fn progress(&self, _project: &str, _completed: u64, _total: u64) {}
    fn progress_workers(&self, project: &str, completed: u64, total: u64, _workers: usize) {
        self.progress(project, completed, total);
    }
    fn progress_pipeline(
        &self,
        project: &str,
        completed: u64,
        total: u64,
        pipeline: &crate::progress::Pipeline,
    ) {
        self.progress_workers(project, completed, total, pipeline.workers);
    }
    fn put(&self, header: &SemanticSnapshot, record: Record, replace: bool) -> Result<()>;
    fn put_batch(
        &self,
        header: &SemanticSnapshot,
        records: Vec<Record>,
        replace: bool,
    ) -> Result<()> {
        for record in records {
            self.put(header, record, replace)?;
        }
        Ok(())
    }
}
pub fn drain(s: &mut SemanticSnapshot, store: &dyn Store) -> Result<()> {
    let records = std::mem::take(&mut s.entities)
        .into_iter()
        .map(Record::Entity)
        .chain(std::mem::take(&mut s.bodies).into_iter().map(Record::Body))
        .chain(
            std::mem::take(&mut s.occurrences)
                .into_iter()
                .map(Record::Occurrence),
        )
        .chain(
            std::mem::take(&mut s.relations)
                .into_iter()
                .map(Record::Relation),
        )
        .chain(
            std::mem::take(&mut s.documents)
                .into_iter()
                .map(Record::Document),
        );
    let mut batch = Vec::with_capacity(128);
    for record in records {
        batch.push(record);
        if batch.len() == 128 {
            store.put_batch(s, std::mem::take(&mut batch), false)?;
        }
    }
    if !batch.is_empty() {
        store.put_batch(s, batch, false)?;
    }
    Ok(())
}
