//! `$jsonSchema` validator generation for declared fields (D15).
//! Undeclared fields stay allowed (`additionalProperties` is not restricted).

use bson::{Bson, Document, doc};

use crate::schema::{CollectionDef, FieldDef, FieldType};

/// The value for `{ $jsonSchema: <this> }`.
pub fn collection_schema(def: &CollectionDef) -> Document {
    object_schema(&def.fields)
}

fn object_schema(fields: &[FieldDef]) -> Document {
    let mut props = Document::new();
    let mut required = Vec::<Bson>::new();
    for f in fields {
        if f.required {
            required.push(Bson::String(f.name.clone()));
        }
        props.insert(f.name.clone(), field_schema(&f.ty, f.nullable));
    }
    let mut out = doc! { "bsonType": "object" };
    if !required.is_empty() {
        out.insert("required", required);
    }
    if !props.is_empty() {
        out.insert("properties", props);
    }
    out
}

fn bson_types(ty: &FieldType) -> Option<Vec<&'static str>> {
    Some(match ty {
        FieldType::ObjectId => vec!["objectId"],
        FieldType::String => vec!["string"],
        FieldType::Int => vec!["int"],
        FieldType::Long => vec!["long"],
        FieldType::Double => vec!["double"],
        FieldType::Decimal => vec!["decimal"],
        FieldType::Number => vec!["int", "long", "double", "decimal"],
        FieldType::Bool => vec!["bool"],
        FieldType::Date => vec!["date"],
        FieldType::Binary | FieldType::Uuid => vec!["binData"],
        FieldType::Array(_) => vec!["array"],
        FieldType::Object(_) => vec!["object"],
        FieldType::Any => return None,
    })
}

fn field_schema(ty: &FieldType, nullable: bool) -> Document {
    let Some(mut types) = bson_types(ty) else {
        return Document::new();
    };
    if nullable {
        types.push("null");
    }
    let mut out = match ty {
        FieldType::Object(inner) => {
            let mut d = object_schema(inner);
            d.remove("bsonType");
            d
        }
        FieldType::Array(inner) => doc! { "items": field_schema(inner, false) },
        _ => Document::new(),
    };
    let bt: Bson = if types.len() == 1 {
        types[0].into()
    } else {
        Bson::Array(types.into_iter().map(Bson::from).collect())
    };
    let mut res = doc! { "bsonType": bt };
    res.extend(std::mem::take(&mut out));
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_nested_schema() {
        let def = CollectionDef {
            name: "c".into(),
            fields: vec![
                FieldDef::new("name", FieldType::String).required(),
                FieldDef::new("age", FieldType::Int).nullable(),
                FieldDef::new("tags", FieldType::Array(Box::new(FieldType::String))),
                FieldDef::new(
                    "addr",
                    FieldType::Object(vec![FieldDef::new("zip", FieldType::String).required()]),
                ),
                FieldDef::new("meta", FieldType::Any),
            ],
            relations: vec![],
        };
        let s = collection_schema(&def);
        assert_eq!(
            s,
            doc! {
                "bsonType": "object",
                "required": ["name"],
                "properties": {
                    "name": { "bsonType": "string" },
                    "age": { "bsonType": ["int", "null"] },
                    "tags": { "bsonType": "array", "items": { "bsonType": "string" } },
                    "addr": { "bsonType": "object", "required": ["zip"],
                              "properties": { "zip": { "bsonType": "string" } } },
                    "meta": {},
                }
            }
        );
    }
}
