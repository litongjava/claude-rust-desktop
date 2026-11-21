//! Request bodies may outlive the last message referencing them. Index only the
//! top-level header references in snapshot arrays, using a connection-local temp
//! table so neither the transcript nor the set of distinct hashes lives in RAM.
use anyhow::Result;
use rusqlite::{Connection, DatabaseName};
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::{fmt, io::BufReader};

pub(super) fn index(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TEMP TABLE export_refs(hash TEXT PRIMARY KEY) WITHOUT ROWID")?;
    let mut rows = db.prepare("SELECT rowid FROM message_trajectory")?;
    for id in rows.query_map([], |r| r.get(0))? {
        let blob = db.blob_open(
            DatabaseName::Main,
            "message_trajectory",
            "requests_json",
            id?,
            true,
        )?;
        let mut json = serde_json::Deserializer::from_reader(BufReader::new(blob));
        Scan::Array(db).deserialize(&mut json)?;
        json.end()?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Scan<'a> {
    Array(&'a Connection),
    Request(&'a Connection),
    Hash(&'a Connection),
}

impl<'de> DeserializeSeed<'de> for Scan<'_> {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(self, de: D) -> Result<(), D::Error> {
        de.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Scan<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a recorded JSON value")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        if let Scan::Array(db) = self {
            while seq.next_element_seed(Scan::Request(db))?.is_some() {}
        } else {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
        }
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        if let Scan::Request(db) = self {
            while let Some(key) = map.next_key::<String>()? {
                if matches!(
                    key.as_str(),
                    "systemPromptRef" | "toolsRef" | "systemMessagesRef"
                ) {
                    map.next_value_seed(Scan::Hash(db))?;
                } else {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        } else {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        }
        Ok(())
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<(), E> {
        if let Scan::Hash(db) = self {
            if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
                db.prepare_cached("INSERT OR IGNORE INTO export_refs VALUES(?1)")
                    .and_then(|mut insert| insert.execute([value]))
                    .map_err(E::custom)?;
            }
        }
        Ok(())
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }
}
