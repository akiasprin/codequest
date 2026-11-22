//! SAX ingestion retains a compact native reference catalogue and disk-backed
//! rows, not the complete provider JSON or a project SemanticSnapshot.
use crate::{Error, Result, model::*, ports::ContentRepository, semantic::records::Store};
use serde::Deserialize;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt,
    io::{BufReader, BufWriter, Write},
    os::unix::fs::FileExt,
    path::Path,
};
mod parallel;
#[cfg(test)]
mod performance;
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Row {
    offset: u64,
    bytes: u64,
}
pub struct Archive {
    metadata: Value,
    rows: Vec<Row>,
    file: std::fs::File,
    _temporary: Option<tempfile::NamedTempFile>,
}
struct Builder {
    metadata: serde_json::Map<String, Value>,
    rows: Vec<Row>,
    writer: BufWriter<std::fs::File>,
    file: tempfile::NamedTempFile,
    offset: u64,
    max_bytes: u64,
}
fn parse_export_limit(value: Option<&str>) -> Result<u64> {
    value.map_or(Ok(8 * 1024 * 1024 * 1024), |v| {
        v.parse::<u64>()
            .ok()
            .filter(|n| *n > 0 && *n <= 16 * 1024 * 1024 * 1024)
            .ok_or_else(|| {
                Error::new(
                    "InvalidProviderBudget",
                    "CQ_NATIVE_EXPORT_MAX_BYTES must be 1..17179869184 bytes",
                )
            })
    })
}
/// Bounds the complete disk-backed export, not an individual stored fact.
pub fn export_limit() -> Result<u64> {
    parse_export_limit(std::env::var("CQ_NATIVE_EXPORT_MAX_BYTES").ok().as_deref())
}

fn sketch(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|k, v| {
                if v.is_string() {
                    match k.as_str() {
                        "key"
                        | "kind"
                        | "path"
                        | "unit"
                        | "from"
                        | "to"
                        | "definition_kind"
                        | "from_definition_kind"
                        | "to_definition_kind" => {}
                        // Preserve required text presence and wrong-typed
                        // supplied contract fields. Removing those would turn
                        // corrupt evidence into apparently absent evidence.
                        "text" | "id" | "token" | "target" | "call_index" | "child" | "owner"
                        | "parent_token" | "file" | "to_file" | "start" | "end" | "external"
                        | "exported" | "local" | "trait_member" | "const_function"
                        | "is_definition" => *v = json!(""),
                        // Full strings remain in the raw row on disk. They do
                        // not need an allocated empty placeholder in every
                        // project-wide planning object.
                        _ => return false,
                    }
                } else {
                    sketch(v);
                }
                true
            });
        }
        Value::Array(a) => {
            for v in a {
                sketch(v);
            }
        }
        Value::String(s) => s.clear(),
        _ => {}
    }
}
const READ_AHEAD_BYTES: usize = 256 * 1024;
/// Worker-local bounded sequential read-ahead. Plans retain provider order
/// within each table, so adjacent references need one pread rather than one
/// syscall per row. Parsed values still belong only to the admitted frame.
struct RowReader<'a> {
    archive: &'a Archive,
    buffer: Vec<u8>,
    offset: u64,
    previous_end: Option<u64>,
    reads: u64,
    bytes: u64,
    rows: u64,
}
impl<'a> RowReader<'a> {
    fn new(archive: &'a Archive) -> Self {
        Self {
            archive,
            buffer: Vec::new(),
            offset: 0,
            previous_end: None,
            reads: 0,
            bytes: 0,
            rows: 0,
        }
    }
    fn hydrate(&mut self, v: &Value) -> Result<Value> {
        let Some(index) = v["__cq_row"].as_u64() else {
            return Ok(v.clone());
        };
        let row = self
            .archive
            .rows
            .get(index as usize)
            .ok_or_else(|| Error::new("InvalidProviderOutput", "native row coordinate"))?;
        self.rows += 1;
        let mut actual = if row.bytes > READ_AHEAD_BYTES as u64 {
            self.reads += 1;
            self.bytes += row.bytes;
            self.archive.row(index as usize)?
        } else {
            let end = row.offset + row.bytes;
            if row.offset < self.offset || end > self.offset + self.buffer.len() as u64 {
                let archive_end = self.archive.rows.last().map_or(0, |r| r.offset + r.bytes);
                // Speculative read-ahead is useful only after observing
                // consecutive rows. Sparse foreign targets read their exact
                // row instead of amplifying I/O by 256 KiB for every jump.
                let limit = if self.previous_end == Some(row.offset) {
                    READ_AHEAD_BYTES as u64
                } else {
                    row.bytes
                };
                let length = (archive_end - row.offset).min(limit) as usize;
                self.buffer.resize(length, 0);
                self.archive
                    .file
                    .read_exact_at(&mut self.buffer, row.offset)?;
                self.offset = row.offset;
                self.reads += 1;
                self.bytes += length as u64;
            }
            let start = (row.offset - self.offset) as usize;
            serde_json::from_slice(&self.buffer[start..start + row.bytes as usize])?
        };
        self.previous_end = Some(row.offset + row.bytes);
        if v.get("call_index").is_some() {
            actual["call_index"] = v["call_index"].clone();
        }
        Ok(actual)
    }
}
impl Builder {
    fn row(&mut self, mut v: Value) -> Result<Value> {
        if v.get("__cq_row").is_some() {
            return Err(Error::new(
                "InvalidProviderOutput",
                "reserved archive coordinate",
            ));
        }
        struct Count<'a> {
            writer: &'a mut BufWriter<std::fs::File>,
            bytes: u64,
        }
        impl Write for Count<'_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let written = self.writer.write(bytes)?;
                self.bytes += written as u64;
                Ok(written)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.writer.flush()
            }
        }
        let offset = self.offset;
        let mut out = Count {
            writer: &mut self.writer,
            bytes: 0,
        };
        serde_json::to_writer(&mut out, &v)?;
        let end = offset + out.bytes;
        self.offset = end;
        if end > self.max_bytes {
            return Err(Error::new(
                "ProviderOutputLimit",
                format!("native archive exceeds {} byte budget", self.max_bytes),
            ));
        }
        let index = self.rows.len();
        self.rows.push(Row {
            offset,
            bytes: end - offset,
        });
        // Scalar array entries have no coordinate field to hydrate later.
        // Keep them unchanged; structured rows can be compacted losslessly.
        if v.is_object() {
            sketch(&mut v);
        }
        if let Some(m) = v.as_object_mut() {
            m.insert("__cq_row".into(), json!(index));
        }
        Ok(v)
    }
}
struct Field<'a> {
    builder: &'a mut Builder,
    key: &'a str,
}
impl<'de> DeserializeSeed<'de> for Field<'_> {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        d: D,
    ) -> std::result::Result<Value, D::Error> {
        d.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Field<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a native export field")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Value, A::Error> {
        let mut values = vec![];
        while let Some(v) = seq.next_element::<Value>()? {
            values.push(self.builder.row(v).map_err(serde::de::Error::custom)?);
        }
        let _ = self.key;
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, map: A) -> std::result::Result<Value, A::Error> {
        Value::deserialize(serde::de::value::MapAccessDeserializer::new(map))
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Value, E> {
        Ok(json!(v))
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> std::result::Result<Value, E> {
        Ok(json!(v))
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> std::result::Result<Value, E> {
        Ok(json!(v))
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Value, E> {
        Ok(json!(v))
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Value, E> {
        Ok(json!(v))
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Value, E> {
        Ok(json!(v))
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
}
struct Top<'a>(&'a mut Builder);
impl<'de> Visitor<'de> for Top<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("native export object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            let value = map.next_value_seed(Field {
                builder: self.0,
                key: &key,
            })?;
            if self.0.metadata.insert(key, value).is_some() {
                return Err(serde::de::Error::custom("duplicate export field"));
            }
        }
        Ok(())
    }
}
impl Archive {
    pub fn retain(&self, directory: &Path) -> Result<()> {
        use std::io::{Seek, SeekFrom};
        std::fs::create_dir_all(directory)?;
        self.file.sync_all()?;
        let target = directory.join("rows");
        if !target.exists() {
            let linked = self
                ._temporary
                .as_ref()
                .map(|t| std::fs::hard_link(t.path(), &target));
            if !matches!(linked, Some(Ok(()))) {
                let mut source = self.file.try_clone()?;
                source.seek(SeekFrom::Start(0))?;
                let mut out = std::fs::File::create(&target)?;
                std::io::copy(&mut source, &mut out)?;
                out.sync_all()?;
            }
        }
        let mut writer = BufWriter::new(std::fs::File::create(directory.join("index.json"))?);
        serde_json::to_writer(&mut writer, &(&self.metadata, &self.rows))?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        std::fs::File::open(directory)?.sync_all()?;
        Ok(())
    }
    pub fn restore(directory: &Path) -> Result<Self> {
        let (metadata, rows): (Value, Vec<Row>) = serde_json::from_reader(BufReader::new(
            std::fs::File::open(directory.join("index.json"))?,
        ))?;
        let file = std::fs::File::open(directory.join("rows"))?;
        let bytes = file.metadata()?.len();
        if rows
            .iter()
            .any(|r| r.offset.checked_add(r.bytes).is_none_or(|end| end > bytes))
        {
            return Err(Error::new(
                "InvalidProviderOutput",
                "retained archive bounds",
            ));
        }
        Ok(Self {
            metadata,
            rows,
            file,
            _temporary: None,
        })
    }
    pub fn validate_reuse_paths(&self, root: &Path, mapping: &[MaterializedInput]) -> Result<()> {
        let paths = mapping
            .iter()
            .map(|m| m.path.as_str())
            .collect::<HashSet<_>>();
        for f in self.metadata["files"].as_array().into_iter().flatten() {
            if let Some(path) = f["path"].as_str()
                && Path::new(path).starts_with(root)
                && !paths.contains(path)
            {
                return Err(Error::new(
                    "NativeReplayBindingMismatch",
                    format!("retained HIR path is outside the captured input map: {path}"),
                ));
            }
        }
        Ok(())
    }
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_progress(path, None)
    }
    pub fn open_with_progress(
        path: &Path,
        progress: Option<crate::progress::Progress>,
    ) -> Result<Self> {
        let max_bytes = export_limit()?;
        if std::fs::metadata(path)?.len() > max_bytes {
            return Err(Error::new(
                "ProviderOutputLimit",
                format!("native export exceeds {max_bytes} byte budget"),
            ));
        }
        let file = tempfile::NamedTempFile::new()?;
        let mut b = Builder {
            metadata: serde_json::Map::new(),
            rows: vec![],
            writer: BufWriter::with_capacity(1024 * 1024, file.reopen()?),
            file,
            offset: 0,
            max_bytes,
        };
        let reader = crate::progress::Reader {
            inner: std::fs::File::open(path)?,
            progress,
            completed: 0,
        };
        let mut deserializer = serde_json::Deserializer::from_reader(BufReader::new(reader));
        serde::Deserializer::deserialize_map(&mut deserializer, Top(&mut b))?;
        deserializer.end()?;
        // Readers use pread on the archive's original handle. Make the entire
        // buffered row journal visible before publishing any row coordinates.
        b.writer.flush()?;
        let archive = Self {
            metadata: Value::Object(b.metadata),
            rows: b.rows,
            file: b.file.reopen()?,
            _temporary: Some(b.file),
        };
        Ok(archive)
    }
    fn row(&self, index: usize) -> Result<Value> {
        let row = self
            .rows
            .get(index)
            .ok_or_else(|| Error::new("InvalidProviderOutput", "native row coordinate"))?;
        let mut b = vec![0; row.bytes as usize];
        self.file.read_exact_at(&mut b, row.offset)?;
        Ok(serde_json::from_slice(&b)?)
    }
    fn hydrate_row(&self, v: Value) -> Result<Value> {
        self.hydrate_row_ref(&v)
    }
    fn hydrate_row_ref(&self, v: &Value) -> Result<Value> {
        if let Some(index) = v["__cq_row"].as_u64() {
            let mut actual = self.row(index as usize)?;
            if v.get("call_index").is_some() {
                actual["call_index"] = v["call_index"].clone();
            }
            return Ok(actual);
        }
        Ok(v.clone())
    }
    pub fn header(&self) -> Result<Value> {
        let mut value = self
            .metadata
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, v)| !v.is_array())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<serde_json::Map<_, _>>();
        let diagnostics = self.metadata["diagnostics"]
            .as_array()
            .map(|a| {
                a.iter()
                    .cloned()
                    .map(|v| self.hydrate_row(v))
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        value.insert("diagnostics".into(), json!(diagnostics));
        Ok(Value::Object(value))
    }
    fn validate_ranges(&self) -> Result<()> {
        let mut ranges = BTreeMap::<u64, BTreeSet<u64>>::new();
        fn walk(v: &Value, out: &mut BTreeMap<u64, BTreeSet<u64>>) -> Result<()> {
            match v {
                Value::Object(m) => {
                    if m.contains_key("start") || m.contains_key("end") {
                        let (Some(file), Some(start), Some(end)) =
                            (v["file"].as_u64(), v["start"].as_u64(), v["end"].as_u64())
                        else {
                            return Err(Error::new(
                                "InvalidProviderOutput",
                                "incomplete native range",
                            ));
                        };
                        if start > end {
                            return Err(Error::new(
                                "InvalidProviderOutput",
                                "reversed native range",
                            ));
                        }
                        out.entry(file).or_default().extend([start, end]);
                    }
                    for v in m.values() {
                        walk(v, out)?;
                    }
                }
                Value::Array(a) => {
                    for v in a {
                        walk(v, out)?;
                    }
                }
                _ => {}
            }
            Ok(())
        }
        walk(&self.metadata, &mut ranges)?;
        for file in self.metadata["files"].as_array().unwrap() {
            let id = file["id"].as_u64().unwrap();
            let Some(offsets) = ranges.remove(&id) else {
                continue;
            };
            let value = self.hydrate_row(file.clone())?;
            let text = value["text"]
                .as_str()
                .ok_or_else(|| Error::new("InvalidProviderOutput", "native file text"))?;
            for offset in offsets {
                if offset > text.len() as u64 || !text.is_char_boundary(offset as usize) {
                    return Err(Error::new(
                        "InvalidProviderOutput",
                        "native range outside file or splits UTF-8",
                    ));
                }
            }
        }
        if !ranges.is_empty() {
            return Err(Error::new(
                "InvalidProviderOutput",
                "native range file absent",
            ));
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub fn normalize(
        &self,
        s: &mut SemanticSnapshot,
        mapping: &[MaterializedInput],
        content: &dyn ContentRepository,
        provider: &str,
        language: Language,
        version: &str,
        sink: &dyn Store,
    ) -> Result<()> {
        self.normalize_policy(s, mapping, content, provider, language, version, sink, true)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn normalize_policy(
        &self,
        s: &mut SemanticSnapshot,
        mapping: &[MaterializedInput],
        content: &dyn ContentRepository,
        provider: &str,
        language: Language,
        version: &str,
        sink: &dyn Store,
        checkpoints: bool,
    ) -> Result<()> {
        super::provider_contract::validate_metadata(&self.metadata)?;
        self.validate_ranges()?;
        parallel::normalize(
            self,
            s,
            mapping,
            content,
            provider,
            language,
            version,
            sink,
            parallel::Config {
                checkpoints,
                ..parallel::Config::from_env()?
            },
        )
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    fn export() -> Value {
        json!({"protocol":"codequest-hir-export","schema_version":1,
            "files":[{"id":0,"path":"/repo/a.rs","text":"αx"}],
            "symbols":[{"token":0,"key":"a","kind":"Function","name":"readable name",
                "documentation":"Author intention", "definition":{"file":0,"start":0,"end":2}}],
            "references":[{"target":0,"kind":"Reference","source":{"file":0,"start":2,"end":3}}],
            "parents":[],"relations":[],"bodies":[],"calls":[],"doc_links":[],
            "diagnostics":["scalar diagnostic",{"message":"structured diagnostic"}]})
    }
    fn open(raw: &Value) -> Archive {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        serde_json::to_writer(&mut file, raw).unwrap();
        Archive::open(file.path()).unwrap()
    }
    #[test]
    fn compact_planning_rows_hydrate_to_the_exact_original() {
        let raw = export();
        let archive = open(&raw);
        assert!(archive.metadata["symbols"][0].get("name").is_none());
        assert!(
            archive.metadata["symbols"][0]
                .get("documentation")
                .is_none()
        );
        assert_eq!(
            archive.metadata["symbols"][0]["definition"],
            raw["symbols"][0]["definition"]
        );
        assert_eq!(archive.metadata["files"][0]["text"], "");
        for (name, value) in raw.as_object().unwrap() {
            if let Some(rows) = value.as_array() {
                for (i, row) in rows.iter().enumerate() {
                    assert_eq!(
                        &archive.hydrate_row_ref(&archive.metadata[name][i]).unwrap(),
                        row
                    );
                }
            }
        }
        assert_eq!(archive.header().unwrap()["diagnostics"], raw["diagnostics"]);
    }
    #[test]
    fn compact_planning_rows_preserve_all_contract_rejections() {
        let mut cases = Vec::new();
        for flag in [
            "external",
            "exported",
            "local",
            "trait_member",
            "const_function",
            "is_definition",
        ] {
            let mut raw = export();
            raw["symbols"][0][flag] = json!("wrong type");
            cases.push(raw);
        }
        for field in ["id", "text", "unit"] {
            let mut raw = export();
            raw["files"][0][field] = json!(37);
            cases.push(raw);
        }
        for field in ["token", "key", "kind"] {
            let mut raw = export();
            raw["symbols"][0][field] = Value::Null;
            cases.push(raw);
        }
        for field in ["target", "is_definition", "call_index"] {
            let mut raw = export();
            raw["references"][0][field] = json!("wrong type");
            cases.push(raw);
        }
        let mut raw = export();
        raw["parents"] =
            json!([{"child":0,"parent_token":"wrong type","key":"parent","kind":"Module"}]);
        cases.push(raw);
        let mut raw = export();
        raw["files"][0]["unit"] = Value::Null;
        cases.push(raw);
        for field in ["file", "start", "end"] {
            let mut raw = export();
            raw["symbols"][0]["definition"][field] = json!("wrong type");
            cases.push(raw);
        }
        for (start, end) in [(1, 2), (2, 1), (0, 4)] {
            let mut raw = export();
            raw["symbols"][0]["definition"]["start"] = json!(start);
            raw["symbols"][0]["definition"]["end"] = json!(end);
            cases.push(raw);
        }
        // Ranges in unknown payload fields must still be checked recursively.
        let mut raw = export();
        raw["symbols"][0]["extra"] = json!({"nested":[{"file":0,"start":"bad","end":2}]});
        cases.push(raw);
        for raw in cases {
            assert!(
                super::super::provider_contract::validate(&raw).is_err(),
                "{raw}"
            );
            let archive = open(&raw);
            let validation = super::super::provider_contract::validate_metadata(&archive.metadata)
                .and_then(|()| archive.validate_ranges());
            assert!(
                validation.is_err(),
                "compaction accepted corrupt input: {raw}"
            );
        }
    }
    #[test]
    fn read_ahead_preserves_sparse_large_and_remapped_rows() {
        let mut raw = export();
        raw["symbols"][0]["documentation"] = json!("z".repeat(READ_AHEAD_BYTES + 1));
        let archive = open(&raw);
        let mut reader = RowReader::new(&archive);
        for name in [
            "references",
            "files",
            "symbols",
            "diagnostics",
            "references",
        ] {
            for (i, original) in raw[name].as_array().unwrap().iter().enumerate() {
                assert_eq!(
                    &reader.hydrate(&archive.metadata[name][i]).unwrap(),
                    original
                );
            }
        }
        let mut row = archive.metadata["references"][0].clone();
        row["call_index"] = json!(3);
        let mut expected = raw["references"][0].clone();
        expected["call_index"] = json!(3);
        assert_eq!(reader.hydrate(&row).unwrap(), expected);
    }
    #[test]
    fn consecutive_rows_share_reads_without_amplifying_sparse_reads() {
        let mut raw = export();
        raw["references"] = json!(
            (0..1024)
                .map(|_| raw["references"][0].clone())
                .collect::<Vec<_>>()
        );
        let archive = open(&raw);
        let mut reader = RowReader::new(&archive);
        for (i, row) in archive.metadata["references"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            assert_eq!(reader.hydrate(row).unwrap(), raw["references"][i]);
        }
        assert_eq!(reader.rows, 1024);
        assert!(reader.reads < 10, "reads={}", reader.reads);
        let first = &archive.metadata["symbols"][0];
        let mut sparse = RowReader::new(&archive);
        sparse.hydrate(first).unwrap();
        let coordinate = first["__cq_row"].as_u64().unwrap() as usize;
        assert_eq!(sparse.bytes, archive.rows[coordinate].bytes);
    }
    #[test]
    fn buffered_journal_preserves_offsets_across_flush_boundaries() {
        let mut raw = export();
        raw["diagnostics"] = json!(
            (0..2400)
                .map(|i| json!({"message":format!("{i}:{}", "α".repeat(512))}))
                .collect::<Vec<_>>()
        );
        let archive = open(&raw);
        assert!(archive.file.metadata().unwrap().len() > 2 * 1024 * 1024);
        let mut reader = RowReader::new(&archive);
        for i in [2399, 0, 1023, 1024, 2398] {
            let coordinate = &archive.metadata["diagnostics"][i];
            assert_eq!(
                archive.hydrate_row_ref(coordinate).unwrap(),
                raw["diagnostics"][i]
            );
            assert_eq!(reader.hydrate(coordinate).unwrap(), raw["diagnostics"][i]);
        }
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut builder = Builder {
            metadata: Default::default(),
            rows: Vec::new(),
            writer: BufWriter::new(file.reopen().unwrap()),
            file,
            offset: 0,
            max_bytes: 1,
        };
        assert_eq!(
            builder
                .row(json!({"message":"over budget"}))
                .unwrap_err()
                .code(),
            "ProviderOutputLimit"
        );
        assert!(builder.rows.is_empty());
    }
    #[test]
    fn export_budget_is_separate_from_fact_value_limits() {
        assert_eq!(parse_export_limit(None).unwrap(), 8 * 1024 * 1024 * 1024);
        assert_eq!(parse_export_limit(Some("1")).unwrap(), 1);
        assert_eq!(
            parse_export_limit(Some("17179869184")).unwrap(),
            16 * 1024 * 1024 * 1024
        );
        for bad in ["0", "-1", "bytes", "17179869185"] {
            assert_eq!(
                parse_export_limit(Some(bad)).unwrap_err().code(),
                "InvalidProviderBudget"
            );
        }
    }
}
