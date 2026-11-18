//! Immutable semantic shard checkpoints. Publication remains guarded by HEAD CAS.
use crate::{LocalStore, atomic::immutable_write};
use cq_core::{
    Error, Result,
    model::*,
    ports::{ContentReader, ContentRepository},
    semantic::generation::{Delta, Generation},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};
const SHARD_BYTES: u64 = 1024 * 1024 * 1024;
const DERIVED_BYTES: u64 = 16 * SHARD_BYTES;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    pub version: u32,
    pub snapshot_revision: String,
    pub coordinates: Generation,
    pub blobs: BTreeMap<String, BlobId>,
    pub digest: String,
}
impl Checkpoint {
    pub fn seal(&mut self) -> Result<()> {
        self.digest.clear();
        self.digest = digest(&serde_json::to_vec(self)?);
        Ok(())
    }
}
impl LocalStore {
    pub fn load_checkpoint(&self, project: &str, revision: &str) -> Result<Option<Generation>> {
        let path = self.revision_path(revision)?.join("checkpoint.json");
        if !path.exists() {
            return Ok(None);
        }
        let mut record: Checkpoint =
            serde_json::from_slice(&read_bounded(&path, 16 * 1024 * 1024)?)?;
        let expected = record.digest.clone();
        record.seal()?;
        if expected != record.digest
            || record.version != 1
            || record.snapshot_revision != revision
            || record.coordinates.project != project
            || !record.coordinates.shards.is_empty()
        {
            return Err(Error::new(
                "InvalidCheckpoint",
                "checkpoint coordinates/digest",
            ));
        }
        if record.blobs.keys().collect::<Vec<_>>()
            != record
                .coordinates
                .manifest
                .iter()
                .map(|m| &m.key)
                .collect::<Vec<_>>()
        {
            return Err(Error::new(
                "InvalidCheckpoint",
                "checkpoint shard inventory",
            ));
        }
        let mut total = 0_u64;
        for blob in record.blobs.values() {
            total = total.checked_add(blob.len).ok_or_else(|| {
                Error::new("InvalidCheckpoint", "derived content budget overflow")
            })?;
            if total > DERIVED_BYTES {
                return Err(Error::new(
                    "InvalidCheckpoint",
                    "derived content budget exceeded",
                ));
            }
        }
        for (key, blob) in record.blobs {
            if blob.len > SHARD_BYTES {
                return Err(Error::new("InvalidCheckpoint", "oversized shard"));
            }
            let bytes = self.content.read_blob(&blob)?;
            record
                .coordinates
                .shards
                .insert(key, serde_json::from_slice(&bytes)?);
        }
        record.coordinates.validate(&self.content)?;
        Ok(Some(record.coordinates))
    }
    pub(crate) fn stage_checkpoint(
        &self,
        snapshot: &SemanticSnapshot,
        prior: Option<&Generation>,
        directory: &Path,
    ) -> Result<()> {
        let projected = cq_core::semantic::generation::project(
            snapshot,
            prior.map(|p| p.sequence + 1).unwrap_or(1),
        )?;
        // Validate the owned projection directly. Building a full Delta and
        // applying it here would clone the complete graph twice before writing
        // the same content-addressed shards.
        projected.validate(&self.content)?;
        let base = prior.filter(|p| p.universe == projected.universe);
        let old: BTreeMap<_, _> = base
            .map(|p| p.manifest.iter().map(|m| (&m.key, &m.digest)).collect())
            .unwrap_or_default();
        let upserts: Vec<_> = projected
            .manifest
            .iter()
            .filter(|m| old.get(&m.key).is_none_or(|d| **d != m.digest))
            .map(|m| m.key.clone())
            .collect();
        let deletes: Vec<_> = old
            .keys()
            .filter(|k| !projected.shards.contains_key(**k))
            .map(|k| (*k).clone())
            .collect();
        let mut record = Checkpoint {
            version: 1,
            snapshot_revision: snapshot.revision.clone(),
            coordinates: projected,
            blobs: BTreeMap::new(),
            digest: String::new(),
        };
        // Separate output accounting from the compiler-input reservation. Both
        // limits remain bounded: 1 GiB per shard, 16 GiB unique derived bytes per
        // publication. Source capture keeps its original request budget.
        let derived = self.content.budget_scope();
        for (key, shard) in std::mem::take(&mut record.coordinates.shards) {
            let bytes = serde_json::to_vec(&shard)?;
            if bytes.len() as u64 > SHARD_BYTES {
                return Err(Error::new(
                    "CheckpointContentLimit",
                    format!("shard exceeds 1 GiB: {key}"),
                ));
            }
            record
                .blobs
                .insert(key, derived.put(&bytes, DERIVED_BYTES)?);
        }
        record.seal()?;
        // Delta payloads refer to the same content-addressed shard blobs. A complete
        // checkpoint can be validated in isolation, while deltas require their base.
        let mut compact = serde_json::json!({"version":record.coordinates.version,"project":record.coordinates.project,"root":record.coordinates.root,"universe":record.coordinates.universe,"sequence":record.coordinates.sequence,"base_generation":base.map(|b|&b.generation),"generation":record.coordinates.generation,"manifest":record.coordinates.manifest,"deletes":deletes,"provenance":record.coordinates.provenance});
        compact["upserts"] = serde_json::to_value(
            upserts
                .iter()
                .map(|key| (key, &record.blobs[key]))
                .collect::<BTreeMap<_, _>>(),
        )?;
        compact["digest"] = serde_json::to_value(digest(&serde_json::to_vec(&compact)?))?;
        immutable_write(
            &directory.join("generation-delta.json"),
            &serde_json::to_vec(&compact)?,
        )?;
        immutable_write(
            &directory.join("checkpoint.json"),
            &serde_json::to_vec(&record)?,
        )?;
        Ok(())
    }
    pub fn generation_delta(&self, project: &str, revision: &str) -> Result<Delta> {
        let current = self.load_checkpoint(project, revision)?.ok_or_else(|| {
            Error::new(
                "MissingCheckpoint",
                "refresh to establish semantic generation",
            )
        })?;
        let mut compact: serde_json::Value = serde_json::from_slice(&read_bounded(
            &self.revision_path(revision)?.join("generation-delta.json"),
            16 * 1024 * 1024,
        )?)?;
        let checksum = compact.as_object_mut().and_then(|v| v.remove("digest"));
        if checksum.as_ref().and_then(|v| v.as_str())
            != Some(digest(&serde_json::to_vec(&compact)?).as_str())
        {
            return Err(Error::new(
                "InvalidCheckpoint",
                "delta publication checksum",
            ));
        }
        let mut value = compact.clone();
        value["upserts"] = serde_json::to_value(
            compact["upserts"]
                .as_object()
                .ok_or_else(|| Error::new("InvalidCheckpoint", "delta upserts"))?
                .keys()
                .map(|key| {
                    current
                        .shards
                        .get(key)
                        .cloned()
                        .ok_or_else(|| Error::new("InvalidCheckpoint", "unknown upsert shard"))
                })
                .collect::<Result<Vec<_>>>()?,
        )?;
        let delta: Delta = serde_json::from_value(value)?;
        if delta.version != current.version
            || delta.project != current.project
            || delta.root != current.root
            || delta.universe != current.universe
            || delta.sequence != current.sequence
            || delta.generation != current.generation
            || delta.manifest != current.manifest
        {
            return Err(Error::new(
                "InvalidCheckpoint",
                "delta disagrees with checkpoint",
            ));
        }
        Ok(delta)
    }
}
fn read_bounded(path: &Path, budget: u64) -> Result<Vec<u8>> {
    if std::fs::metadata(path)?.len() > budget {
        return Err(Error::new("InvalidCheckpoint", "checkpoint size limit"));
    }
    Ok(std::fs::read(path)?)
}

impl LocalStore {
    pub fn read_native_cache(&self, key: &str) -> Result<Option<NativeCache>> {
        cache_key(key)?;
        let path = self.root.join("native-cache").join(format!("{key}.json"));
        if !path.exists() {
            return Ok(None);
        }
        let value: serde_json::Value =
            serde_json::from_slice(&read_bounded(&path, 16 * 1024 * 1024)?)?;
        if value["digest"].as_str() != Some(&digest(&serde_json::to_vec(&value["cache"])?)) {
            return Err(Error::new("InvalidCheckpoint", "native cache digest"));
        }
        let cache: NativeCache = serde_json::from_value(value["cache"].clone())?;
        if cache.key != key {
            return Err(Error::new("InvalidCheckpoint", "foreign native cache"));
        }
        Ok(Some(cache))
    }
    pub fn write_native_cache(&self, cache: &NativeCache) -> Result<()> {
        cache_key(&cache.key)?;
        let dir = self.root.join("native-cache");
        std::fs::create_dir_all(&dir)?;
        let value = serde_json::to_value(cache)?;
        let bytes = serde_json::to_vec(
            &serde_json::json!({"digest":digest(&serde_json::to_vec(&value)?),"cache":value}),
        )?;
        let temp = tempfile::NamedTempFile::new_in(&dir)?;
        std::fs::write(temp.path(), bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(dir.join(format!("{}.json", cache.key)))
            .map_err(|e| e.error)?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    }
}
fn cache_key(key: &str) -> Result<()> {
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::new("InvalidCheckpoint", "cache key"));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputCheckpoint {
    pub version: u32,
    pub revision: String,
    pub fingerprint: String,
    pub request: serde_json::Value,
    pub digest: String,
}
impl LocalStore {
    pub fn stage_index_inputs(
        &self,
        revision: &str,
        fingerprint: &str,
        request: &serde_json::Value,
    ) -> Result<()> {
        let directory = self.revision_path(revision)?;
        std::fs::create_dir_all(&directory)?;
        let mut stamp = InputCheckpoint {
            version: 1,
            revision: revision.into(),
            fingerprint: fingerprint.into(),
            request: request.clone(),
            digest: String::new(),
        };
        stamp.digest = digest(&serde_json::to_vec(&stamp)?);
        immutable_write(
            &directory.join("input-checkpoint.json"),
            &serde_json::to_vec(&stamp)?,
        )
    }
    pub fn index_inputs(&self, revision: &str) -> Result<Option<InputCheckpoint>> {
        let path = self.revision_path(revision)?.join("input-checkpoint.json");
        if !path.exists() {
            return Ok(None);
        }
        let mut stamp: InputCheckpoint =
            serde_json::from_slice(&read_bounded(&path, 16 * 1024 * 1024)?)?;
        let expected = std::mem::take(&mut stamp.digest);
        if stamp.version != 1
            || stamp.revision != revision
            || expected != digest(&serde_json::to_vec(&stamp)?)
        {
            return Err(Error::new(
                "InvalidCheckpoint",
                "input/request checkpoint checksum",
            ));
        }
        stamp.digest = expected;
        Ok(Some(stamp))
    }
    pub fn index_request(&self, project: &str) -> Result<serde_json::Value> {
        if let Some(head) = self.head(project)? {
            if let Ok(Some(stamp)) = self.index_inputs(&head) {
                return Ok(stamp.request);
            }
            if let Some(request) = self.load_semantics(project, Some(&head))?.analysis_request {
                return Ok(request);
            }
        }
        Ok(serde_json::from_slice(&std::fs::read(
            self.project_path(project).join("index-request.json"),
        )?)?)
    }
}
