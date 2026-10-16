//! Write-side validation and coercion of declared fields (D7).
//!
//! Values are coerced where it is lossless or clearly intended (e.g. an
//! `Int32` into a `long` field, a 24-hex string into an `object_id` field) so
//! that JSON clients can write typed data. Undeclared fields are untouched.

use std::str::FromStr;

use bson::{Binary, Bson, Decimal128, Document, oid::ObjectId, spec::BinarySubtype};

use crate::error::Violation;
use crate::schema::{CollectionDef, FieldDef, FieldType};

/// Validate and coerce a full document for insertion.
pub fn validate_document(def: &CollectionDef, doc: &mut Document) -> Result<(), Vec<Violation>> {
    let mut out = vec![];
    check_object(&def.fields, doc, "", &mut out);
    if out.is_empty() { Ok(()) } else { Err(out) }
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}.{name}")
    }
}

fn check_object(fields: &[FieldDef], doc: &mut Document, prefix: &str, out: &mut Vec<Violation>) {
    for f in fields {
        let path = join(prefix, &f.name);
        match doc.get_mut(&f.name) {
            None => {
                if f.required && !(prefix.is_empty() && f.name == "_id") {
                    out.push(Violation::new(path, "is required"));
                }
            }
            Some(v) => check_value(&f.ty, f.nullable, v, &path, out),
        }
    }
}

/// Check (and coerce in place) a value against a declared type.
pub fn check_value(
    ty: &FieldType,
    nullable: bool,
    v: &mut Bson,
    path: &str,
    out: &mut Vec<Violation>,
) {
    if matches!(v, Bson::Null) {
        if !nullable && !matches!(ty, FieldType::Any) {
            out.push(Violation::new(path, "must not be null"));
        }
        return;
    }
    let ok = match ty {
        FieldType::Any => true,
        FieldType::ObjectId => match v {
            Bson::ObjectId(_) => true,
            Bson::String(s) => match ObjectId::parse_str(s.as_str()) {
                Ok(oid) => {
                    *v = Bson::ObjectId(oid);
                    true
                }
                Err(_) => false,
            },
            _ => false,
        },
        FieldType::String => matches!(v, Bson::String(_)),
        FieldType::Int => match v {
            Bson::Int32(_) => true,
            Bson::Int64(n) => match i32::try_from(*n) {
                Ok(n) => {
                    *v = Bson::Int32(n);
                    true
                }
                Err(_) => false,
            },
            _ => false,
        },
        FieldType::Long => match v {
            Bson::Int64(_) => true,
            Bson::Int32(n) => {
                *v = Bson::Int64(i64::from(*n));
                true
            }
            _ => false,
        },
        FieldType::Double => match v {
            Bson::Double(_) => true,
            Bson::Int32(n) => {
                *v = Bson::Double(f64::from(*n));
                true
            }
            Bson::Int64(n) => {
                *v = Bson::Double(*n as f64);
                true
            }
            _ => false,
        },
        FieldType::Decimal => match v {
            Bson::Decimal128(_) => true,
            Bson::Int32(_) | Bson::Int64(_) | Bson::String(_) => {
                let s = match v {
                    Bson::Int32(n) => n.to_string(),
                    Bson::Int64(n) => n.to_string(),
                    Bson::String(s) => s.clone(),
                    _ => unreachable!(),
                };
                match Decimal128::from_str(&s) {
                    Ok(d) => {
                        *v = Bson::Decimal128(d);
                        true
                    }
                    Err(_) => false,
                }
            }
            _ => false,
        },
        FieldType::Number => {
            matches!(
                v,
                Bson::Int32(_) | Bson::Int64(_) | Bson::Double(_) | Bson::Decimal128(_)
            )
        }
        FieldType::Bool => matches!(v, Bson::Boolean(_)),
        FieldType::Date => matches!(v, Bson::DateTime(_)),
        FieldType::Binary => matches!(v, Bson::Binary(_)),
        FieldType::Uuid => match v {
            Bson::Binary(b) => b.subtype == BinarySubtype::Uuid,
            Bson::String(s) => match bson::Uuid::parse_str(s.as_str()) {
                Ok(u) => {
                    *v = Bson::Binary(Binary::from_uuid(u));
                    true
                }
                Err(_) => false,
            },
            _ => false,
        },
        FieldType::Array(inner) => match v {
            Bson::Array(items) => {
                for (i, item) in items.iter_mut().enumerate() {
                    check_value(inner, false, item, &format!("{path}.{i}"), out);
                }
                true
            }
            _ => false,
        },
        FieldType::Object(fields) => match v {
            Bson::Document(d) => {
                check_object(fields, d, path, out);
                true
            }
            _ => false,
        },
    };
    if !ok {
        out.push(Violation::new(
            path,
            format!("expected {}, got {}", type_name(ty), bson_name(v)),
        ));
    }
}

pub fn type_name(ty: &FieldType) -> String {
    match ty {
        FieldType::ObjectId => "object_id".into(),
        FieldType::String => "string".into(),
        FieldType::Int => "int".into(),
        FieldType::Long => "long".into(),
        FieldType::Double => "double".into(),
        FieldType::Decimal => "decimal".into(),
        FieldType::Number => "number".into(),
        FieldType::Bool => "bool".into(),
        FieldType::Date => "date".into(),
        FieldType::Binary => "binary".into(),
        FieldType::Uuid => "uuid".into(),
        FieldType::Any => "any".into(),
        FieldType::Array(t) => format!("array<{}>", type_name(t)),
        FieldType::Object(_) => "object".into(),
    }
}

fn bson_name(v: &Bson) -> &'static str {
    match v {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Array(_) => "array",
        Bson::Document(_) => "object",
        Bson::Boolean(_) => "bool",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) => "javascript",
        Bson::Int32(_) => "int",
        Bson::Int64(_) => "long",
        Bson::Timestamp(_) => "timestamp",
        Bson::Binary(_) => "binary",
        Bson::ObjectId(_) => "object_id",
        Bson::DateTime(_) => "date",
        Bson::Symbol(_) => "symbol",
        Bson::Decimal128(_) => "decimal",
        Bson::Undefined => "undefined",
        Bson::MaxKey => "max_key",
        Bson::MinKey => "min_key",
        Bson::DbPointer(_) => "db_pointer",
    }
}

const NUMERIC_OPS: &[&str] = &["$inc", "$mul"];
const SET_OPS: &[&str] = &["$set", "$setOnInsert"];
const ARRAY_ADD_OPS: &[&str] = &["$push", "$addToSet"];
const ALLOWED_OPS: &[&str] = &[
    "$set",
    "$setOnInsert",
    "$unset",
    "$inc",
    "$mul",
    "$min",
    "$max",
    "$rename",
    "$currentDate",
    "$push",
    "$addToSet",
    "$pull",
    "$pullAll",
    "$pop",
    "$bit",
];

/// Validate (and coerce) an operator-style update against declared fields.
/// Paths that do not resolve to a declared field (array positions, undeclared
/// fields) are left to the database-side `$jsonSchema` validator.
pub fn validate_update(def: &CollectionDef, update: &mut Document) -> Result<(), Vec<Violation>> {
    let mut out = vec![];
    if update.is_empty() {
        out.push(Violation::new("", "update must not be empty"));
    }
    for (op, spec) in update.iter_mut() {
        if !ALLOWED_OPS.contains(&op.as_str()) {
            out.push(Violation::new(op.clone(), "unsupported update operator"));
            continue;
        }
        let Bson::Document(spec) = spec else {
            out.push(Violation::new(
                op.clone(),
                "operator argument must be a document",
            ));
            continue;
        };
        for (path, value) in spec.iter_mut() {
            if path == "_id" || path.starts_with("_id.") {
                out.push(Violation::new(path.clone(), "`_id` is immutable"));
                continue;
            }
            let Some(field) = def.field_at_path(path) else {
                continue;
            };
            let op = op.as_str();
            if SET_OPS.contains(&op) {
                check_value(&field.ty, field.nullable, value, path, &mut out);
            } else if op == "$unset" {
                if field.required {
                    out.push(Violation::new(
                        path.clone(),
                        "is required and cannot be unset",
                    ));
                }
            } else if NUMERIC_OPS.contains(&op) {
                if !matches!(
                    field.ty,
                    FieldType::Int
                        | FieldType::Long
                        | FieldType::Double
                        | FieldType::Decimal
                        | FieldType::Number
                        | FieldType::Any
                ) {
                    out.push(Violation::new(
                        path.clone(),
                        format!(
                            "`{op}` needs a numeric field, field is {}",
                            type_name(&field.ty)
                        ),
                    ));
                }
            } else if op == "$rename" {
                out.push(Violation::new(
                    path.clone(),
                    "declared fields cannot be renamed",
                ));
            } else if ARRAY_ADD_OPS.contains(&op) {
                match &field.ty {
                    FieldType::Array(inner) => {
                        let items: Vec<&mut Bson> = match value {
                            Bson::Document(d) if d.contains_key("$each") => {
                                match d.get_mut("$each") {
                                    Some(Bson::Array(a)) => a.iter_mut().collect(),
                                    _ => vec![],
                                }
                            }
                            other => vec![other],
                        };
                        for item in items {
                            check_value(inner, false, item, path, &mut out);
                        }
                    }
                    FieldType::Any => {}
                    other => out.push(Violation::new(
                        path.clone(),
                        format!("`{op}` needs an array field, field is {}", type_name(other)),
                    )),
                }
            }
        }
    }
    if out.is_empty() { Ok(()) } else { Err(out) }
}

/// Paths an operator update may modify. `None` means "unknown / everything".
pub fn touched_paths(update: &Document) -> Option<Vec<String>> {
    let mut out = vec![];
    for (op, spec) in update {
        if !op.starts_with('$') {
            return None;
        }
        let Bson::Document(spec) = spec else {
            return None;
        };
        for (path, value) in spec {
            // Strip positional segments: `a.$.b` / `a.$[x].b` / `a.0.b` -> `a`.
            let clean: Vec<&str> = path
                .split('.')
                .take_while(|s| !s.starts_with('$') && s.parse::<usize>().is_err())
                .collect();
            if clean.is_empty() {
                return None;
            }
            out.push(clean.join("."));
            if op == "$rename" {
                if let Bson::String(to) = value {
                    out.push(to.clone());
                }
            }
        }
    }
    Some(out)
}

/// Does an update (by its touched paths) modify `path`?
pub fn touches(touched: &Option<Vec<String>>, path: &str) -> bool {
    match touched {
        None => true,
        Some(paths) => paths.iter().any(|p| crate::path::paths_overlap(p, path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    fn def() -> CollectionDef {
        CollectionDef {
            name: "c".into(),
            fields: vec![
                FieldDef::new("name", FieldType::String).required(),
                FieldDef::new("n", FieldType::Long),
                FieldDef::new("ref", FieldType::ObjectId).nullable(),
                FieldDef::new("tags", FieldType::Array(Box::new(FieldType::String))),
                FieldDef::new("price", FieldType::Decimal),
            ],
            relations: vec![],
        }
    }

    #[test]
    fn coerces_and_reports() {
        let mut d = doc! { "name": "x", "n": 5, "ref": "65a1b2c3d4e5f60718293a4b", "price": 3 };
        validate_document(&def(), &mut d).unwrap();
        assert!(matches!(d.get("n"), Some(Bson::Int64(5))));
        assert!(matches!(d.get("ref"), Some(Bson::ObjectId(_))));
        assert!(matches!(d.get("price"), Some(Bson::Decimal128(_))));

        let mut bad = doc! { "n": "five", "tags": ["a", 1], "extra": { "anything": true } };
        let errs = validate_document(&def(), &mut bad).unwrap_err();
        let paths: Vec<_> = errs.iter().map(|v| v.path.as_str()).collect();
        assert_eq!(paths, vec!["name", "n", "tags.1"]);
    }

    #[test]
    fn validates_updates() {
        let mut u = doc! { "$set": { "n": 1 }, "$push": { "tags": { "$each": ["a"] } } };
        validate_update(&def(), &mut u).unwrap();
        assert!(matches!(
            u.get_document("$set").unwrap().get("n"),
            Some(Bson::Int64(1))
        ));

        let mut bad = doc! { "$unset": { "name": "" }, "$inc": { "tags": 1 }, "$where": {} };
        let errs = validate_update(&def(), &mut bad).unwrap_err();
        assert_eq!(errs.len(), 3);
    }

    #[test]
    fn touched() {
        let u = doc! { "$set": { "a.b": 1, "arr.$.x": 2, "list.3": 1 }, "$rename": { "c": "d" } };
        let t = touched_paths(&u);
        assert_eq!(
            t,
            Some(vec![
                "a.b".into(),
                "arr".into(),
                "list".into(),
                "c".into(),
                "d".into()
            ])
        );
        assert!(touches(&t, "a"));
        assert!(touches(&t, "arr.x"));
        assert!(!touches(&t, "b"));
    }
}
