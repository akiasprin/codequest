//! Resumable full-audit stages; each uses the original independent checks.
use super::*;
use crate::raw::Image;
use cq_core::{execution::Stage, ports::facts::ObjectRef};
use serde::{Deserialize, Serialize};
use std::io::{BufReader, BufWriter};
pub(super) mod entries {
    use serde::{
        Deserialize, Deserializer, Serialize, Serializer,
        de::{SeqAccess, Visitor},
        ser::SerializeSeq,
    };
    use std::{collections::HashMap, hash::Hash, marker::PhantomData};
    pub fn serialize<K: Serialize, V: Serialize, S: Serializer>(
        map: &HashMap<K, V>,
        s: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(map.len()))?;
        for pair in map {
            seq.serialize_element(&pair)?;
        }
        seq.end()
    }
    pub fn deserialize<
        'de,
        K: Deserialize<'de> + Eq + Hash,
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    >(
        d: D,
    ) -> std::result::Result<HashMap<K, V>, D::Error> {
        struct Map<K, V>(PhantomData<(K, V)>);
        impl<'de, K: Deserialize<'de> + Eq + Hash, V: Deserialize<'de>> Visitor<'de> for Map<K, V> {
            type Value = HashMap<K, V>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("unique map entries")
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut map = HashMap::new();
                while let Some((k, v)) = seq.next_element()? {
                    if map.insert(k, v).is_some() {
                        return Err(serde::de::Error::custom("duplicate checkpoint key"));
                    }
                }
                Ok(map)
            }
        }
        d.deserialize_seq(Map(PhantomData))
    }
}
#[derive(Serialize, Deserialize)]
struct Receipt {
    version: u32,
    stage: Stage,
    manifest: String,
    data: Image,
    objects: Image,
    summary: Value,
}
fn fingerprint(path: &Path) -> Result<Image> {
    let mut hash = Sha256::new();
    let bytes = std::io::copy(&mut File::open(path)?, &mut hash)?;
    Ok(Image {
        path: path.to_owned(),
        bytes,
        sha256: format!("{:x}", hash.finalize()),
    })
}
fn validate(image: &Image, directory: &Path) -> Result<()> {
    let path = image.path.canonicalize()?;
    if !path.starts_with(directory.canonicalize()?) {
        return Err(Error::new(
            "InvalidAuditCheckpoint",
            "artifact outside audit directory",
        ));
    }
    let actual = fingerprint(&path)?;
    if actual.bytes != image.bytes || actual.sha256 != image.sha256 {
        return Err(Error::new("DigestMismatch", "audit checkpoint"));
    }
    Ok(())
}
fn retain<T: Serialize>(directory: &Path, value: &T) -> Result<Image> {
    crate::atomic::create_directory(directory)?;
    let mut tmp = tempfile::NamedTempFile::new_in(directory)?;
    {
        let mut out = lz4_flex::frame::FrameEncoder::new(BufWriter::new(tmp.as_file_mut()));
        serde_json::to_writer(&mut out, value)?;
        let mut writer = out.finish().map_err(|e| Error::Io(e.into()))?;
        writer.flush()?;
    }
    tmp.as_file().sync_all()?;
    let hash = fingerprint(tmp.path())?;
    let path = directory.join(&hash.sha256);
    if !path.exists() {
        std::fs::hard_link(tmp.path(), &path)?;
    }
    File::open(directory)?.sync_all()?;
    Ok(Image { path, ..hash })
}
fn restore<T: serde::de::DeserializeOwned>(directory: &Path, image: &Image) -> Result<T> {
    validate(image, directory)?;
    Ok(serde_json::from_reader(
        lz4_flex::frame::FrameDecoder::new(BufReader::new(File::open(&image.path)?)),
    )?)
}
impl StoreHost {
    fn audit_directory(&self, r: &Revision) -> PathBuf {
        self.root.join("audit-stages").join(&r.seal)
    }
    fn audit_receipt(&self, r: &Revision, stage: Stage) -> Result<Receipt> {
        let directory = self.audit_directory(r);
        let pointer: Image = serde_json::from_reader(File::open(
            directory.join(format!("{}.json", stage.name())),
        )?)?;
        let receipt: Receipt = restore(&directory, &pointer)?;
        if receipt.version != 1 || receipt.stage != stage || receipt.manifest != r.seal {
            return Err(Error::new("InvalidAuditCheckpoint", "receipt binding"));
        }
        Ok(receipt)
    }
    fn audit_save<T: Serialize>(
        &self,
        r: &Revision,
        stage: Stage,
        value: &T,
        objects: Vec<ObjectRef>,
        summary: Value,
    ) -> Result<Value> {
        let directory = self.audit_directory(r);
        let data = retain(&directory, value)?;
        let objects = retain(&directory, &objects)?;
        let receipt = retain(
            &directory,
            &Receipt {
                version: 1,
                stage,
                manifest: r.seal.clone(),
                data,
                objects,
                summary: summary.clone(),
            },
        )?;
        save(&directory.join(format!("{}.json", stage.name())), &receipt)?;
        Ok(json!({"receipt":receipt.path,"summary":summary,"manifest":r.seal}))
    }
    pub fn audit_stage(&self, r: &Revision, stage: Stage) -> Result<Value> {
        self.audit_stage_controlled(r, stage, &cq_core::control::CaptureControl::default())
    }
    pub(crate) fn audit_stage_controlled(
        &self,
        r: &Revision,
        stage: Stage,
        control: &cq_core::control::CaptureControl,
    ) -> Result<Value> {
        let _g = self.publication.lock().unwrap();
        let _objects = retention::ObjectGate::lock(self.objects.root.parent().unwrap())?;
        r.verify()?;
        if r.project != self.project {
            return Err(Error::new("WrongProject", &r.project));
        }
        self.point(&format!("stage-{}-started", stage.name()))?;
        self.objects.clear_cache();
        self.objects.clear_index_cache();
        self.objects.trace_begin();
        let result = self.audit_stage_inner(r, stage, control);
        let refs = self.objects.trace_end();
        let (data, summary) = result?;
        self.audit_save(r, stage, &data, refs, summary)
    }
    fn audit_stage_inner(
        &self,
        r: &Revision,
        stage: Stage,
        control: &cq_core::control::CaptureControl,
    ) -> Result<(Value, Value)> {
        let root = crate::facts::limits::root(&self.objects.root).unwrap_or(&self.root);
        let directory = self.audit_directory(r);
        match stage {
            Stage::AuditInventory => {
                let total = r.parent.is_none().then_some(r.changes.facts_changed);
                let mut inventory = cq_core::progress::Progress::stage(
                    root,
                    &r.project,
                    "audit.inventory",
                    "审计清点事实",
                    total,
                    "事实",
                );
                let mut count = 0;
                let mut pages = BTreeMap::<String, crate::facts::inventory::Page<Key>>::new();
                tree::visit(&self.objects, &r.roots.facts, |key, v| {
                    if control.is_cancelled() {
                        return Err(Error::new("Cancelled", "audit inventory paused"));
                    }
                    let f: FactRef = serde_json::from_value(v.clone())?;
                    count += 1;
                    inventory.count(count);
                    crate::facts::inventory::push(&mut pages, key, f, reference_hash(v)?)?;
                    Ok(())
                })?;
                let data = retain(&directory, &(count, pages))?;
                Ok((serde_json::to_value(data)?, json!({"facts":count})))
            }
            Stage::AuditPayloads => {
                let receipt = self.audit_receipt(r, Stage::AuditInventory)?;
                let image: Image = restore(&directory, &receipt.data)?;
                let (count, pages): (u64, BTreeMap<String, crate::facts::inventory::Page<Key>>) =
                    restore(&directory, &image)?;
                let page_count = pages.len();
                let mut proof = Proof {
                    indexed: r.metadata["search_index"]["version"] == 1,
                    // The inventory was counted independently. Reserve once rather
                    // than keeping two multi-gigabyte tables alive during rehashing.
                    facts: HashMap::with_capacity(usize::try_from(count).map_err(|_| {
                        Error::new("AuditReadBudget", "fact count exceeds address space")
                    })?),
                    ..Proof::default()
                };
                let mut progress = cq_core::progress::Progress::stage_with_workers(
                    root,
                    &r.project,
                    "audit.pages",
                    "审计事实载荷",
                    Some(page_count as u64),
                    "页",
                    cq_core::compute::workers().unwrap_or(1),
                );
                let mut completed = 0;
                let mut pages = pages.into_values();
                loop {
                    if control.is_cancelled() {
                        return Err(Error::new("Cancelled", "audit payloads paused"));
                    }
                    let wave = pages
                        .by_ref()
                        .take(cq_core::compute::workers()?.min(8))
                        .collect::<Vec<_>>();
                    if wave.is_empty() {
                        break;
                    }
                    let values = cq_core::compute::map(wave, |page| {
                        let bytes = self.objects.audit_bytes(&page.object)?;
                        cq_core::compute::map(page.rows, |row| {
                            let (key, f, fingerprint) = row.expand(&page.object);
                            let value = writer::decode_bytes(&bytes, &f)?;
                            let family = key.split(':').next().unwrap();
                            let names = cq_core::ports::facts::groups_indexed(
                                family,
                                &value,
                                proof.indexed,
                            )
                            .into_iter()
                            .map(|name| hash(name.as_bytes()))
                            .collect();
                            let kh = hash(key.as_bytes());
                            Ok((key, fingerprint, value, kh, names))
                        })
                    })?;
                    for rows in values {
                        for (key, fingerprint, value, kh, names) in rows {
                            proof.record(key, fingerprint, value, kh, names)?;
                        }
                        completed += 1;
                        progress.count(completed);
                    }
                }
                for (key, (id, code)) in std::mem::take(&mut proof.required) {
                    if !proof.facts.contains_key(&key) {
                        return Err(Error::new(
                            if code == 1 {
                                "DanglingEndpoint"
                            } else {
                                "MissingProvenance"
                            },
                            id,
                        ));
                    }
                }
                let facts = proof.facts.len();
                let data = retain(&directory, &proof)?;
                Ok((
                    serde_json::to_value(data)?,
                    json!({"facts":facts,"payload_pages":page_count}),
                ))
            }
            Stage::AuditDirectories => {
                let receipt = self.audit_receipt(r, Stage::AuditPayloads)?;
                let image: Image = restore(&directory, &receipt.data)?;
                let mut proof: Proof = restore(&directory, &image)?;
                let mut progress = cq_core::progress::Progress::stage_with_workers(
                    root,
                    &r.project,
                    "audit.directories",
                    "审计检索目录",
                    Some(proof.groups.iter().map(|g| g.expected).sum()),
                    "成员",
                    cq_core::compute::workers().unwrap_or(1),
                );
                let mut directories = Vec::with_capacity(DIRECTORY_WAVE);
                let mut directory_peak = 0;
                let mut directory_stats = directories::DirectoryStats::default();
                let mut completed = 0;
                tree::visit(&self.objects, &r.roots.groups, |name, value| {
                    if control.is_cancelled() {
                        return Err(Error::new("Cancelled", "audit directories paused"));
                    }
                    directories.push((
                        name.to_owned(),
                        serde_json::from_value::<writer::Group>(value.clone())?,
                    ));
                    directory_peak = directory_peak.max(directories.len());
                    if directories.len() == DIRECTORY_WAVE {
                        directory_stats.add(proof.directories_controlled(
                            &self.objects,
                            std::mem::take(&mut directories),
                            &mut progress,
                            &mut completed,
                            Some(control),
                        )?);
                    }
                    Ok(())
                })?;
                directory_stats.add(proof.directories_controlled(
                    &self.objects,
                    directories,
                    &mut progress,
                    &mut completed,
                    Some(control),
                )?);
                if proof.groups.iter().any(|g| !g.seen) {
                    return Err(Error::new("IndexIncomplete", "missing canonical directory"));
                }
                Ok((
                    Value::Null,
                    json!({"directories":proof.groups.len(),"maximum_buffered_directories":directory_peak,"directory_execution":directory_stats,"proof_group_members":proof.group_members.len()}),
                ))
            }
            Stage::AuditScopes => {
                self.audit_receipt(r, Stage::AuditDirectories)?;
                let receipt = self.audit_receipt(r, Stage::AuditPayloads)?;
                let image: Image = restore(&directory, &receipt.data)?;
                let mut proof: Proof = restore(&directory, &image)?;
                let mut sources = Vec::new();
                tree::visit(&self.objects, &r.roots.sources, |_, v| {
                    sources.push(serde_json::from_value::<crate::facts::source::Source>(
                        v.clone(),
                    )?);
                    Ok(())
                })?;
                let mut progress = cq_core::progress::Progress::stage_with_workers(
                    root,
                    &r.project,
                    "audit.sources",
                    "审计冻结源码",
                    Some(sources.len() as u64),
                    "文件",
                    cq_core::compute::workers().unwrap_or(1),
                );
                let mut completed = 0;
                for wave in sources.chunks(cq_core::compute::workers()?.min(8)) {
                    if control.is_cancelled() {
                        return Err(Error::new("Cancelled", "audit sources paused"));
                    }
                    cq_core::compute::map(wave.iter().collect(), |source| {
                        source.audit(&self.objects)
                    })?;
                    completed += wave.len() as u64;
                    progress.count(completed);
                }
                if r.metadata["scope_protocol"] == 2 {
                    proof.scopes_controlled(
                        &self.objects,
                        r.metadata["production"]["partial"] == true,
                        root,
                        &r.project,
                        Some(control),
                    )?;
                }
                Ok((
                    Value::Null,
                    json!({"facts":proof.facts.len(),"scope_protocol":r.metadata["scope_protocol"],"sources_checked":completed}),
                ))
            }
            _ => Err(Error::new("UnknownStage", stage.name())),
        }
    }
    pub(crate) fn cached_audit(&self, r: &Revision) -> Result<Option<Value>> {
        if !self.audit_directory(r).join("audit_scopes.json").exists() {
            return Ok(None);
        }
        let directory = self.audit_directory(r);
        let mut summary = serde_json::Map::new();
        let mut objects = BTreeMap::new();
        for stage in [
            Stage::AuditInventory,
            Stage::AuditPayloads,
            Stage::AuditDirectories,
            Stage::AuditScopes,
        ] {
            let receipt = self.audit_receipt(r, stage)?;
            validate(&receipt.data, &directory)?;
            if matches!(stage, Stage::AuditInventory | Stage::AuditPayloads) {
                let image: Image = restore(&directory, &receipt.data)?;
                validate(&image, &directory)?;
            }
            let refs: Vec<ObjectRef> = restore(&directory, &receipt.objects)?;
            for object in refs {
                objects.insert(object.hash.clone(), object);
            }
            summary.insert(stage.name().into(), receipt.summary);
        }
        // Recheck physical evidence, bypassing payload/index caches. Resuming
        // never turns an old proof into permission to trust changed bytes.
        self.objects.clear_cache();
        self.objects.clear_index_cache();
        for wave in objects.into_values().collect::<Vec<_>>().chunks(256) {
            cq_core::compute::map(wave.iter().collect(), |object| self.objects.audit(object))?;
        }
        Ok(Some(Value::Object(summary)))
    }
}
