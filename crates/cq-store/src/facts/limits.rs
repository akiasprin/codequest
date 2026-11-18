//! Unsupported oversized data is reported with frozen original locations. No
//! identity or value is silently truncated to make a candidate publishable.
use cq_core::{Error, Result, model::*};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
pub fn report(
    root: Option<&Path>,
    family: &str,
    id: &str,
    value: &Value,
    bytes: usize,
    header: Option<&SemanticSnapshot>,
) -> Result<Error> {
    let mut fields = vec![];
    let mut anchors = vec![];
    fn walk(v: &Value, path: &str, fields: &mut Vec<Value>, anchors: &mut Vec<Value>) {
        match v {
            Value::String(s)if s.len()>=4096=>fields.push(json!({"path":path,"category":"Value","role":if path=="/id"||path=="/provider_id"{"identity"}else{"content"},"bytes":s.len(),"digest":digest(s.as_bytes()),"preview":s.chars().take(120).collect::<String>()})),
            Value::Object(m)=>{
                if v["file_revision"].is_string()&&v["start"].is_u64()&&v["end"].is_u64(){anchors.push(v.clone());}
                for(k,v)in m{if k.len()>=4096{fields.push(json!({"path":path,"category":"Value","role":"object_property_name","bytes":k.len(),"digest":digest(k.as_bytes()),"preview":k.chars().take(120).collect::<String>()}));}walk(v,&format!("{path}/{}",if k.len()>4096{format!("long-key:{}",digest(k.as_bytes()))}else{k.replace('~',"~0").replace('/',"~1")}),fields,anchors);}
            },
            Value::Array(a)=>for(i,v)in a.iter().enumerate(){walk(v,&format!("{path}/{i}"),fields,anchors);},
            _=>{}
        }
    }
    walk(value, "", &mut fields, &mut anchors);
    fields.sort_by_key(|v| std::cmp::Reverse(v["bytes"].as_u64().unwrap_or(0)));
    fields.truncate(16);
    let originals=anchors.iter().map(|a|{
        let f=header.and_then(|h|h.sources.files.iter().find(|f|a["file_revision"]==f.id));
        json!({"anchor":a,"logical_path":f.map(|f|f.logical_path.display()),"frozen_blob":f.map(|f|&f.blob),"frozen_path":f.and_then(|f|root.map(|r|r.join("blobs").join(&f.blob.digest)))})
    }).collect::<Vec<_>>();
    let native_exports=header.map(|h|h.providers.iter().map(|p|{
        let raw=p.protocol.split("raw=").nth(1).map(|s|s.split_whitespace().next().unwrap_or(""));
        json!({"provider":p,"frozen_path":root.and_then(|r|raw.map(|hash|r.join("blobs").join(hash)))})
    }).collect::<Vec<_>>());
    // Key is the actual store key (family:id), not a property name inside the
    // JSON payload. Native provider identifiers are Values unless used as id.
    let key_bytes = family.len() + 1 + id.len();
    if key_bytes > super::objects::MAX_OBJECT {
        fields.insert(0,json!({"path":"$storage_key","category":"Key","bytes":key_bytes,"digest":digest(format!("{family}:{id}").as_bytes()),"preview":id.chars().take(120).collect::<String>()}));
    }
    let data = json!({"version":1,"kind":"unsupported_oversized_record","family":family,"id":(id.len()<=4096).then_some(id),"identity":{"bytes":id.len(),"digest":digest(id.as_bytes()),"preview":id.chars().take(120).collect::<String>()},"key_bytes":key_bytes,"value_bytes":bytes,"record_bytes":bytes,"limit_bytes":super::objects::MAX_OBJECT,"fields":fields,"originals":originals,"native_exports":native_exports,"action":"read frozen original ranges or retained native export; do not truncate keys or publish incomplete facts"});
    let path: Option<PathBuf> = root.map(|r| {
        r.join("limit-reports").join(format!(
            "{}.json",
            digest(&serde_json::to_vec(&data).unwrap_or_default())
        ))
    });
    if let Some(p) = &path {
        crate::revisions::save(p, &data)?;
    }
    Ok(Error::new(
        "OversizedFact",
        serde_json::to_string(&json!({"report":path,"details":data}))?,
    ))
}
pub fn root(objects: &Path) -> Option<&Path> {
    let parent = objects.parent()?;
    (parent.file_name()? == "format-2")
        .then(|| parent.parent())
        .flatten()
}
