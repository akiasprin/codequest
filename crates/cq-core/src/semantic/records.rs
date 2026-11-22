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
    fn phase_progress(
        &self,
        _project: &str,
        _phase: &str,
        _label: &str,
        _done: u64,
        _total: Option<u64>,
        _workers: usize,
    ) {
    }
    /// A visitor must not rely on hash-key ordering. Storage can visit immutable
    /// payloads in physical order and decode a bounded batch in parallel.
    fn visit_batches(
        &self,
        family: Option<&str>,
        visitor: &mut dyn FnMut(Vec<Record>) -> Result<()>,
    ) -> Result<()> {
        let mut batch = Vec::with_capacity(512);
        let mut accept = |r| {
            batch.push(r);
            if batch.len() == 512 {
                visitor(std::mem::take(&mut batch))?;
            }
            Ok(())
        };
        if let Some(family) = family {
            self.visit_family(family, &mut accept)?;
        } else {
            self.visit(&mut accept)?;
        }
        if !batch.is_empty() {
            visitor(batch)?;
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub fn map_phase<T: Send>(
    source: &dyn Source,
    family: Option<&str>,
    project: &str,
    phase: &str,
    label: &str,
    transform: impl Fn(Record) -> Result<T> + Sync + Send,
    mut consume: impl FnMut(Vec<T>) -> Result<()>,
) -> Result<()> {
    let total = if let Some(f) = family {
        source.family_len(f)?
    } else {
        ["entity", "body", "occurrence", "relation", "document"]
            .iter()
            .map(|f| source.family_len(f))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .try_fold(0u64, |n, v| v.map(|v| n + v))
    };
    let workers = crate::compute::pool()?.current_num_threads();
    let mut done = 0;
    source.phase_progress(project, phase, label, 0, total, workers);
    source.visit_batches(family, &mut |rows| {
        let count = rows.len() as u64;
        consume(crate::compute::map(rows, &transform)?)?;
        done += count;
        source.phase_progress(project, phase, label, done, total, workers);
        Ok(())
    })
}
pub trait Store: Source {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn normalization_cursor(&self) -> Result<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
    fn checkpoint_normalization(
        &self,
        _header: &SemanticSnapshot,
        _done: u64,
        _total: u64,
        _peak: u64,
    ) -> Result<()> {
        Ok(())
    }

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
