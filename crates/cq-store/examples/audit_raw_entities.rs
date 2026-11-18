//! Diagnose a retained, concatenated analysis journal without re-indexing.
use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, MapAccess, Visitor},
};
use std::{collections::HashMap, fmt, io::BufReader};
#[derive(Deserialize)]
struct Entity {
    id: String,
    unit: String,
    provider_id: String,
    qualified_name: String,
    parent: Option<String>,
    #[serde(default)]
    type_definitions: Vec<String>,
    source: Option<serde_json::Value>,
}
struct Row(Option<Entity>);
impl<'de> Deserialize<'de> for Row {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Row;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("tagged analysis row")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Row, A::Error> {
                let mut entity = false;
                let mut row = None;
                while let Some(k) = a.next_key::<String>()? {
                    match k.as_str() {
                        "family" => entity = a.next_value::<String>()? == "Entity",
                        "value" if entity => row = Some(a.next_value::<Entity>()?),
                        _ => {
                            a.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(Row(row))
            }
        }
        d.deserialize_map(V)
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os().nth(1).ok_or("journal path required")?;
    let mut entities = HashMap::new();
    let mut rows = 0u64;
    let reader = BufReader::with_capacity(1024 * 1024, std::fs::File::open(path)?);
    for row in serde_json::Deserializer::from_reader(reader).into_iter::<Row>() {
        rows += 1;
        if let Some(e) = row?.0 {
            entities.insert(e.id.clone(), e);
        }
    }
    let mut missing = vec![];
    let mut missing_count = 0;
    for e in entities.values() {
        for (field, id) in e
            .parent
            .iter()
            .map(|v| ("parent", v))
            .chain(e.type_definitions.iter().map(|v| ("type_definition", v)))
        {
            if !entities.contains_key(id) {
                missing_count += 1;
                if missing.len() < 32 {
                    missing.push(serde_json::json!({"field":field,"target":id,"entity":e.id,"unit":e.unit,"provider_id":e.provider_id,"qualified_name":e.qualified_name,"source":e.source}));
                }
            }
        }
    }
    println!(
        "{}",
        serde_json::json!({"rows":rows,"entities":entities.len(),"missing_count":missing_count,"examples":missing})
    );
    Ok(())
}
