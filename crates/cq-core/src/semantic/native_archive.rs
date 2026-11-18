//! SAX ingestion retains a compact native reference catalogue and disk-backed
//! rows, not the complete provider JSON or a project SemanticSnapshot.
use crate::{Error, Result, model::*, ports::ContentRepository, semantic::records::Store};
use serde::Deserialize;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt,
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::Mutex,
};
#[derive(Clone)]
struct Row {
    offset: u64,
    bytes: u64,
}
pub struct Archive {
    metadata: Value,
    rows: Vec<Row>,
    file: Mutex<tempfile::NamedTempFile>,
}
struct Builder {
    metadata: serde_json::Map<String, Value>,
    rows: Vec<Row>,
    file: tempfile::NamedTempFile,
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
            for (k, v) in m {
                if v.is_string()
                    && !matches!(
                        k.as_str(),
                        "key"
                            | "kind"
                            | "path"
                            | "unit"
                            | "from"
                            | "to"
                            | "definition_kind"
                            | "from_definition_kind"
                            | "to_definition_kind"
                    )
                {
                    *v = json!("");
                } else {
                    sketch(v);
                }
            }
        }
        Value::Array(a) => {
            for v in a {
                sketch(v);
            }
        }
        _ => {}
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
        let offset = self.file.as_file_mut().stream_position()?;
        {
            let mut out = BufWriter::new(self.file.as_file_mut());
            serde_json::to_writer(&mut out, &v)?;
            out.flush()?;
        }
        let end = self.file.as_file_mut().stream_position()?;
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
        sketch(&mut v);
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
        let mut b = Builder {
            metadata: serde_json::Map::new(),
            rows: vec![],
            file: tempfile::NamedTempFile::new()?,
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
        let archive = Self {
            metadata: Value::Object(b.metadata),
            rows: b.rows,
            file: Mutex::new(b.file),
        };
        Ok(archive)
    }
    fn row(&self, index: usize) -> Result<Value> {
        let row = self
            .rows
            .get(index)
            .ok_or_else(|| Error::new("InvalidProviderOutput", "native row coordinate"))?;
        let mut file = self.file.lock().unwrap();
        file.as_file_mut().seek(SeekFrom::Start(row.offset))?;
        let mut b = vec![0; row.bytes as usize];
        file.as_file_mut().read_exact(&mut b)?;
        Ok(serde_json::from_slice(&b)?)
    }
    fn hydrate_row(&self, v: Value) -> Result<Value> {
        if let Some(index) = v["__cq_row"].as_u64() {
            let mut actual = self.row(index as usize)?;
            if v.get("call_index").is_some() {
                actual["call_index"] = v["call_index"].clone();
            }
            return Ok(actual);
        }
        Ok(v)
    }
    fn hydrate(&self, mut v: Value) -> Result<Value> {
        // Coordinates belong only to top-level array rows. Provider-owned
        // nested JSON must never be interpreted as archive control data.
        if let Some(m) = v.as_object_mut() {
            for value in m.values_mut() {
                if let Some(a) = value.as_array_mut() {
                    for row in a {
                        *row = self.hydrate_row(std::mem::take(row))?;
                    }
                }
            }
        }
        Ok(v)
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
        super::provider_contract::validate_metadata(&self.metadata)?;
        self.validate_ranges()?;
        let files = self.metadata["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_u64())
            .chain(std::iter::once(None));
        let mut peak = 0;
        let mut first = true;
        let mut gaps = HashSet::new();
        let catalogue = super::native_batch::Catalogue::new(&self.metadata);
        let mut source_state = super::native_batch::FrameSources::new(s, mapping);
        let total = self.metadata["files"].as_array().unwrap().len() as u64 + 1;
        sink.progress(&s.project, 0, total);
        let result: Result<()> = (|| {
            for (done, file) in files.enumerate() {
                let selected = catalogue.frame(file)?;
                let frame = self.hydrate(selected)?;
                super::native_batch::emit_frame(
                    s,
                    &frame,
                    mapping,
                    content,
                    provider,
                    language,
                    version,
                    sink,
                    file,
                    &mut peak,
                    &mut first,
                    &mut gaps,
                    &mut source_state,
                )?;
                sink.progress(&s.project, done as u64 + 1, total);
            }
            Ok(())
        })();
        source_state.finish(s);
        result?;
        super::native_batch::record_transport(
            s,
            peak,
            self.metadata["files"].as_array().unwrap().len() + 1,
        );

        Ok(())
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
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
