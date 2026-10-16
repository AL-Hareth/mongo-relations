//! Serde helpers that represent BSON values as relaxed Extended JSON in
//! human-readable formats (JSON) and as native BSON otherwise.
//!
//! Use with `#[serde(with = "mr_core::ejson::bson")]` on `Bson` fields,
//! `ejson::document` on `Document` fields and `ejson::bson_vec` on `Vec<Bson>`.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

pub mod bson {
    use super::*;
    use ::bson::Bson;

    pub fn serialize<S: Serializer>(v: &Bson, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            v.clone().into_relaxed_extjson().serialize(s)
        } else {
            v.serialize(s)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Bson, D::Error> {
        if d.is_human_readable() {
            let v = serde_json::Value::deserialize(d)?;
            Bson::try_from(v).map_err(D::Error::custom)
        } else {
            Bson::deserialize(d)
        }
    }
}

pub mod bson_vec {
    use super::*;
    use ::bson::Bson;

    pub fn serialize<S: Serializer>(v: &[Bson], s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            let arr: Vec<_> = v.iter().map(|b| b.clone().into_relaxed_extjson()).collect();
            arr.serialize(s)
        } else {
            v.serialize(s)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Bson>, D::Error> {
        if d.is_human_readable() {
            let v = Vec::<serde_json::Value>::deserialize(d)?;
            v.into_iter()
                .map(|j| Bson::try_from(j).map_err(D::Error::custom))
                .collect()
        } else {
            Vec::<Bson>::deserialize(d)
        }
    }
}

pub mod document {
    use super::*;
    use ::bson::{Bson, Document};

    pub fn serialize<S: Serializer>(v: &Document, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            Bson::Document(v.clone())
                .into_relaxed_extjson()
                .serialize(s)
        } else {
            v.serialize(s)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Document, D::Error> {
        if d.is_human_readable() {
            let v = serde_json::Value::deserialize(d)?;
            match Bson::try_from(v).map_err(D::Error::custom)? {
                Bson::Document(doc) => Ok(doc),
                other => Err(D::Error::custom(format!(
                    "expected a document, got {other}"
                ))),
            }
        } else {
            Document::deserialize(d)
        }
    }
}

pub mod document_vec {
    use super::*;
    use ::bson::{Bson, Document};

    pub fn serialize<S: Serializer>(v: &[Document], s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            let arr: Vec<_> = v
                .iter()
                .map(|d| Bson::Document(d.clone()).into_relaxed_extjson())
                .collect();
            arr.serialize(s)
        } else {
            v.serialize(s)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Document>, D::Error> {
        if d.is_human_readable() {
            let v = Vec::<serde_json::Value>::deserialize(d)?;
            v.into_iter()
                .map(|j| match Bson::try_from(j).map_err(D::Error::custom)? {
                    Bson::Document(doc) => Ok(doc),
                    other => Err(D::Error::custom(format!(
                        "expected a document, got {other}"
                    ))),
                })
                .collect()
        } else {
            Vec::<Document>::deserialize(d)
        }
    }
}

/// Convert a relaxed/canonical Extended JSON value into BSON.
pub fn from_json(v: serde_json::Value) -> Result<::bson::Bson, String> {
    ::bson::Bson::try_from(v).map_err(|e| e.to_string())
}

/// Convert a BSON value into relaxed Extended JSON.
pub fn to_json(v: ::bson::Bson) -> serde_json::Value {
    v.into_relaxed_extjson()
}
