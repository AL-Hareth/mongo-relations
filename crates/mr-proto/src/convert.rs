//! `core <-> pb` conversions. Core -> pb is infallible; pb -> core validates
//! enums, oneofs and BSON payloads.

use bson::{Bson, Document, doc};
use mr_core::query::{CmpOp, Expr, Include, JoinStrategy, Query, SortKey};
use mr_core::schema::{
    CollectionDef, EmbedDef, FieldDef, FieldType, ID_FIELD, Integrity, Junction, OnDelete,
    RelationDef, RelationKind, Schema, Via,
};
use mr_core::write::{CascadeCount, WriteOp, WriteResult};

use crate::pb;

/// A malformed protobuf message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertError(pub String);

impl std::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid request: {}", self.0)
    }
}

impl std::error::Error for ConvertError {}

type Result<T> = std::result::Result<T, ConvertError>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(ConvertError(msg.into()))
}

// ------------------------------------------------------------------ BSON

pub fn encode_doc(d: &Document) -> Vec<u8> {
    let mut buf = Vec::new();
    d.to_writer(&mut buf)
        .expect("writing BSON to a Vec cannot fail");
    buf
}

pub fn decode_doc(bytes: &[u8]) -> Result<Document> {
    Document::from_reader(bytes).map_err(|e| ConvertError(format!("bad BSON document: {e}")))
}

/// Encode a single value as `{ "v": value }`.
pub fn encode_value(v: Bson) -> Vec<u8> {
    encode_doc(&doc! { "v": v })
}

pub fn decode_value(bytes: &[u8]) -> Result<Bson> {
    let mut d = decode_doc(bytes)?;
    d.remove("v")
        .ok_or_else(|| ConvertError("BSON value wrapper lacks `v`".into()))
}

fn decode_values(bytes: &[u8]) -> Result<Vec<Bson>> {
    match decode_value(bytes)? {
        Bson::Array(a) => Ok(a),
        other => err(format!("expected an array of values, got {other}")),
    }
}

fn or_id(s: String) -> String {
    if s.is_empty() {
        ID_FIELD.to_string()
    } else {
        s
    }
}

fn empty_if_id(s: &str) -> String {
    if s == ID_FIELD {
        String::new()
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------- schema

pub fn schema_to_pb(s: &Schema) -> pb::Schema {
    pb::Schema {
        collections: s.collections.iter().map(collection_to_pb).collect(),
    }
}

fn collection_to_pb(c: &CollectionDef) -> pb::Collection {
    pb::Collection {
        name: c.name.clone(),
        fields: c.fields.iter().map(field_to_pb).collect(),
        relations: c.relations.iter().map(relation_to_pb).collect(),
    }
}

fn field_to_pb(f: &FieldDef) -> pb::Field {
    pb::Field {
        name: f.name.clone(),
        r#type: Some(type_to_pb(&f.ty)),
        required: f.required,
        nullable: f.nullable,
        unique: f.unique,
    }
}

fn type_to_pb(t: &FieldType) -> pb::FieldType {
    use pb::field_type::Kind;
    let scalar = |s: pb::Scalar| Kind::Scalar(s as i32);
    let kind = match t {
        FieldType::ObjectId => scalar(pb::Scalar::ObjectId),
        FieldType::String => scalar(pb::Scalar::String),
        FieldType::Int => scalar(pb::Scalar::Int),
        FieldType::Long => scalar(pb::Scalar::Long),
        FieldType::Double => scalar(pb::Scalar::Double),
        FieldType::Decimal => scalar(pb::Scalar::Decimal),
        FieldType::Number => scalar(pb::Scalar::Number),
        FieldType::Bool => scalar(pb::Scalar::Bool),
        FieldType::Date => scalar(pb::Scalar::Date),
        FieldType::Binary => scalar(pb::Scalar::Binary),
        FieldType::Uuid => scalar(pb::Scalar::Uuid),
        FieldType::Any => scalar(pb::Scalar::Any),
        FieldType::Array(inner) => Kind::Array(Box::new(type_to_pb(inner))),
        FieldType::Object(fields) => Kind::Object(pb::ObjectType {
            fields: fields.iter().map(field_to_pb).collect(),
        }),
    };
    pb::FieldType { kind: Some(kind) }
}

fn relation_to_pb(r: &RelationDef) -> pb::Relation {
    use pb::relation::Kind;
    let fk = |foreign_key: &str, references: &str| pb::ForeignKey {
        foreign_key: foreign_key.to_string(),
        references: empty_if_id(references),
    };
    let kind = match &r.kind {
        RelationKind::BelongsTo {
            foreign_key,
            references,
        } => Kind::BelongsTo(fk(foreign_key, references)),
        RelationKind::HasOne {
            foreign_key,
            references,
        } => Kind::HasOne(fk(foreign_key, references)),
        RelationKind::HasMany {
            foreign_key,
            references,
        } => Kind::HasMany(fk(foreign_key, references)),
        RelationKind::ManyToMany { via, references } => {
            use pb::many_to_many::Via as PVia;
            Kind::ManyToMany(pb::ManyToMany {
                via: Some(match via {
                    Via::Local(f) => PVia::Local(f.clone()),
                    Via::Remote(f) => PVia::Remote(f.clone()),
                    Via::Junction(j) => PVia::Junction(pb::Junction {
                        collection: j.collection.clone(),
                        local_key: j.local_key.clone(),
                        target_key: j.target_key.clone(),
                    }),
                }),
                references: empty_if_id(references),
            })
        }
    };
    pb::Relation {
        name: r.name.clone(),
        target: r.target.clone(),
        kind: Some(kind),
        integrity: match r.integrity {
            None => pb::Integrity::Unspecified,
            Some(Integrity::Strict) => pb::Integrity::Strict,
            Some(Integrity::Loose) => pb::Integrity::Loose,
        } as i32,
        on_delete: match r.on_delete {
            None => pb::OnDelete::Unspecified,
            Some(OnDelete::Restrict) => pb::OnDelete::Restrict,
            Some(OnDelete::Cascade) => pb::OnDelete::Cascade,
            Some(OnDelete::SetNull) => pb::OnDelete::SetNull,
            Some(OnDelete::NoAction) => pb::OnDelete::NoAction,
        } as i32,
        embed: r.embed.as_ref().map(|e| pb::Embed {
            fields: e.fields.clone(),
            into: e.into.clone(),
        }),
    }
}

pub fn schema_from_pb(s: pb::Schema) -> Result<Schema> {
    Ok(Schema {
        collections: s
            .collections
            .into_iter()
            .map(collection_from_pb)
            .collect::<Result<_>>()?,
    })
}

fn collection_from_pb(c: pb::Collection) -> Result<CollectionDef> {
    Ok(CollectionDef {
        name: c.name,
        fields: c
            .fields
            .into_iter()
            .map(field_from_pb)
            .collect::<Result<_>>()?,
        relations: c
            .relations
            .into_iter()
            .map(relation_from_pb)
            .collect::<Result<_>>()?,
    })
}

fn field_from_pb(f: pb::Field) -> Result<FieldDef> {
    let Some(t) = f.r#type else {
        return err(format!("field `{}` has no type", f.name));
    };
    Ok(FieldDef {
        ty: type_from_pb(t, &f.name)?,
        name: f.name,
        required: f.required,
        nullable: f.nullable,
        unique: f.unique,
    })
}

fn type_from_pb(t: pb::FieldType, name: &str) -> Result<FieldType> {
    use pb::field_type::Kind;
    Ok(match t.kind {
        None => return err(format!("field `{name}` has no type")),
        Some(Kind::Scalar(s)) => match pb::Scalar::try_from(s) {
            Ok(pb::Scalar::ObjectId) => FieldType::ObjectId,
            Ok(pb::Scalar::String) => FieldType::String,
            Ok(pb::Scalar::Int) => FieldType::Int,
            Ok(pb::Scalar::Long) => FieldType::Long,
            Ok(pb::Scalar::Double) => FieldType::Double,
            Ok(pb::Scalar::Decimal) => FieldType::Decimal,
            Ok(pb::Scalar::Number) => FieldType::Number,
            Ok(pb::Scalar::Bool) => FieldType::Bool,
            Ok(pb::Scalar::Date) => FieldType::Date,
            Ok(pb::Scalar::Binary) => FieldType::Binary,
            Ok(pb::Scalar::Uuid) => FieldType::Uuid,
            Ok(pb::Scalar::Any) => FieldType::Any,
            Ok(pb::Scalar::Unspecified) | Err(_) => {
                return err(format!("field `{name}` has an unknown scalar type"));
            }
        },
        Some(Kind::Array(inner)) => FieldType::Array(Box::new(type_from_pb(*inner, name)?)),
        Some(Kind::Object(o)) => FieldType::Object(
            o.fields
                .into_iter()
                .map(field_from_pb)
                .collect::<Result<_>>()?,
        ),
    })
}

fn relation_from_pb(r: pb::Relation) -> Result<RelationDef> {
    use pb::relation::Kind;
    let kind = match r.kind {
        None => return err(format!("relation `{}` has no kind", r.name)),
        Some(Kind::BelongsTo(f)) => RelationKind::BelongsTo {
            foreign_key: f.foreign_key,
            references: or_id(f.references),
        },
        Some(Kind::HasOne(f)) => RelationKind::HasOne {
            foreign_key: f.foreign_key,
            references: or_id(f.references),
        },
        Some(Kind::HasMany(f)) => RelationKind::HasMany {
            foreign_key: f.foreign_key,
            references: or_id(f.references),
        },
        Some(Kind::ManyToMany(m)) => {
            use pb::many_to_many::Via as PVia;
            let via = match m.via {
                None => return err(format!("relation `{}` has no `via`", r.name)),
                Some(PVia::Local(f)) => Via::Local(f),
                Some(PVia::Remote(f)) => Via::Remote(f),
                Some(PVia::Junction(j)) => Via::Junction(Junction {
                    collection: j.collection,
                    local_key: j.local_key,
                    target_key: j.target_key,
                }),
            };
            RelationKind::ManyToMany {
                via,
                references: or_id(m.references),
            }
        }
    };
    let integrity = match pb::Integrity::try_from(r.integrity) {
        Ok(pb::Integrity::Unspecified) => None,
        Ok(pb::Integrity::Strict) => Some(Integrity::Strict),
        Ok(pb::Integrity::Loose) => Some(Integrity::Loose),
        Err(_) => return err(format!("relation `{}` has an unknown integrity", r.name)),
    };
    let on_delete = match pb::OnDelete::try_from(r.on_delete) {
        Ok(pb::OnDelete::Unspecified) => None,
        Ok(pb::OnDelete::Restrict) => Some(OnDelete::Restrict),
        Ok(pb::OnDelete::Cascade) => Some(OnDelete::Cascade),
        Ok(pb::OnDelete::SetNull) => Some(OnDelete::SetNull),
        Ok(pb::OnDelete::NoAction) => Some(OnDelete::NoAction),
        Err(_) => return err(format!("relation `{}` has an unknown on_delete", r.name)),
    };
    Ok(RelationDef {
        name: r.name,
        target: r.target,
        kind,
        integrity,
        on_delete,
        embed: r.embed.map(|e| EmbedDef {
            fields: e.fields,
            into: e.into,
        }),
    })
}

// ----------------------------------------------------------------- query

fn fields_to_pb(f: &Option<Vec<String>>) -> Option<pb::FieldList> {
    f.as_ref().map(|f| pb::FieldList { fields: f.clone() })
}

fn sort_to_pb(s: &[SortKey]) -> Vec<pb::SortKey> {
    s.iter()
        .map(|k| pb::SortKey {
            field: k.field.clone(),
            desc: k.desc,
        })
        .collect()
}

fn sort_from_pb(s: Vec<pb::SortKey>) -> Vec<SortKey> {
    s.into_iter()
        .map(|k| SortKey {
            field: k.field,
            desc: k.desc,
        })
        .collect()
}

pub fn query_to_pb(q: &Query) -> pb::Query {
    pb::Query {
        collection: q.collection.clone(),
        filter: q.filter.as_ref().map(expr_to_pb),
        select: fields_to_pb(&q.select),
        include: q.include.iter().map(include_to_pb).collect(),
        sort: sort_to_pb(&q.sort),
        skip: q.skip,
        limit: q.limit,
    }
}

fn include_to_pb(i: &Include) -> pb::Include {
    pb::Include {
        relation: i.relation.clone(),
        alias: i.alias.clone(),
        filter: i.filter.as_ref().map(expr_to_pb),
        select: fields_to_pb(&i.select),
        sort: sort_to_pb(&i.sort),
        limit: i.limit,
        required: i.required,
        include: i.include.iter().map(include_to_pb).collect(),
        strategy: match i.strategy {
            None => pb::JoinStrategy::Auto,
            Some(JoinStrategy::Lookup) => pb::JoinStrategy::Lookup,
            Some(JoinStrategy::Batch) => pb::JoinStrategy::Batch,
        } as i32,
    }
}

pub fn expr_to_pb(e: &Expr) -> pb::Expr {
    use pb::expr::Kind;
    let list = |v: &[Expr]| pb::ExprList {
        exprs: v.iter().map(expr_to_pb).collect(),
    };
    let kind = match e {
        Expr::And(v) => Kind::And(list(v)),
        Expr::Or(v) => Kind::Or(list(v)),
        Expr::Not(x) => Kind::Not(Box::new(expr_to_pb(x))),
        Expr::Cmp { path, op, value } => Kind::Cmp(pb::Cmp {
            path: path.clone(),
            op: match op {
                CmpOp::Eq => pb::CmpOp::Eq,
                CmpOp::Ne => pb::CmpOp::Ne,
                CmpOp::Gt => pb::CmpOp::Gt,
                CmpOp::Gte => pb::CmpOp::Gte,
                CmpOp::Lt => pb::CmpOp::Lt,
                CmpOp::Lte => pb::CmpOp::Lte,
            } as i32,
            value: encode_value(value.clone()),
        }),
        Expr::In { path, values } => Kind::In(pb::InList {
            path: path.clone(),
            values: encode_value(Bson::Array(values.clone())),
        }),
        Expr::Nin { path, values } => Kind::Nin(pb::InList {
            path: path.clone(),
            values: encode_value(Bson::Array(values.clone())),
        }),
        Expr::Exists { path, exists } => Kind::Exists(pb::Exists {
            path: path.clone(),
            exists: *exists,
        }),
        Expr::IsNull { path } => Kind::IsNull(path.clone()),
        Expr::Regex {
            path,
            pattern,
            options,
        } => Kind::Regex(pb::Regex {
            path: path.clone(),
            pattern: pattern.clone(),
            options: options.clone(),
        }),
        Expr::Any { relation, filter } => Kind::Any(Box::new(pb::Quantified {
            relation: relation.clone(),
            filter: filter.as_ref().map(|f| Box::new(expr_to_pb(f))),
        })),
        Expr::All { relation, filter } => Kind::All(Box::new(pb::Quantified {
            relation: relation.clone(),
            filter: Some(Box::new(expr_to_pb(filter))),
        })),
        Expr::Raw(d) => Kind::Raw(encode_doc(d)),
    };
    pb::Expr { kind: Some(kind) }
}

pub fn query_from_pb(q: pb::Query) -> Result<Query> {
    Ok(Query {
        collection: q.collection,
        filter: q.filter.map(expr_from_pb).transpose()?,
        select: q.select.map(|f| f.fields),
        include: q
            .include
            .into_iter()
            .map(include_from_pb)
            .collect::<Result<_>>()?,
        sort: sort_from_pb(q.sort),
        skip: q.skip,
        limit: q.limit,
    })
}

fn include_from_pb(i: pb::Include) -> Result<Include> {
    Ok(Include {
        strategy: match pb::JoinStrategy::try_from(i.strategy) {
            Ok(pb::JoinStrategy::Auto) => None,
            Ok(pb::JoinStrategy::Lookup) => Some(JoinStrategy::Lookup),
            Ok(pb::JoinStrategy::Batch) => Some(JoinStrategy::Batch),
            Err(_) => return err(format!("include `{}` has an unknown strategy", i.relation)),
        },
        relation: i.relation,
        alias: i.alias,
        filter: i.filter.map(expr_from_pb).transpose()?,
        select: i.select.map(|f| f.fields),
        sort: sort_from_pb(i.sort),
        limit: i.limit,
        required: i.required,
        include: i
            .include
            .into_iter()
            .map(include_from_pb)
            .collect::<Result<_>>()?,
    })
}

pub fn expr_from_pb(e: pb::Expr) -> Result<Expr> {
    use pb::expr::Kind;
    let list = |l: pb::ExprList| {
        l.exprs
            .into_iter()
            .map(expr_from_pb)
            .collect::<Result<Vec<_>>>()
    };
    Ok(match e.kind {
        None => return err("empty filter expression"),
        Some(Kind::And(l)) => Expr::And(list(l)?),
        Some(Kind::Or(l)) => Expr::Or(list(l)?),
        Some(Kind::Not(x)) => Expr::Not(Box::new(expr_from_pb(*x)?)),
        Some(Kind::Cmp(c)) => Expr::Cmp {
            op: match pb::CmpOp::try_from(c.op) {
                Ok(pb::CmpOp::Eq) => CmpOp::Eq,
                Ok(pb::CmpOp::Ne) => CmpOp::Ne,
                Ok(pb::CmpOp::Gt) => CmpOp::Gt,
                Ok(pb::CmpOp::Gte) => CmpOp::Gte,
                Ok(pb::CmpOp::Lt) => CmpOp::Lt,
                Ok(pb::CmpOp::Lte) => CmpOp::Lte,
                Ok(pb::CmpOp::Unspecified) | Err(_) => {
                    return err(format!("comparison on `{}` has no operator", c.path));
                }
            },
            value: decode_value(&c.value)?,
            path: c.path,
        },
        Some(Kind::In(l)) => Expr::In {
            values: decode_values(&l.values)?,
            path: l.path,
        },
        Some(Kind::Nin(l)) => Expr::Nin {
            values: decode_values(&l.values)?,
            path: l.path,
        },
        Some(Kind::Exists(x)) => Expr::Exists {
            path: x.path,
            exists: x.exists,
        },
        Some(Kind::IsNull(path)) => Expr::IsNull { path },
        Some(Kind::Regex(r)) => Expr::Regex {
            path: r.path,
            pattern: r.pattern,
            options: r.options,
        },
        Some(Kind::Any(q)) => Expr::Any {
            relation: q.relation,
            filter: q
                .filter
                .map(|f| expr_from_pb(*f).map(Box::new))
                .transpose()?,
        },
        Some(Kind::All(q)) => {
            let Some(f) = q.filter else {
                return err(format!("`all` on `{}` needs a filter", q.relation));
            };
            Expr::All {
                relation: q.relation,
                filter: Box::new(expr_from_pb(*f)?),
            }
        }
        Some(Kind::Raw(b)) => Expr::Raw(decode_doc(&b)?),
    })
}

// ----------------------------------------------------------------- write

pub fn write_op_to_pb(op: &WriteOp) -> pb::WriteOp {
    use pb::write_op::Op;
    let op = match op {
        WriteOp::Insert {
            collection,
            documents,
        } => Op::Insert(pb::Insert {
            collection: collection.clone(),
            documents: documents.iter().map(encode_doc).collect(),
        }),
        WriteOp::Update {
            collection,
            filter,
            update,
            multi,
        } => Op::Update(pb::Update {
            collection: collection.clone(),
            filter: filter.as_ref().map(expr_to_pb),
            update: encode_doc(update),
            single: !multi,
        }),
        WriteOp::Delete {
            collection,
            filter,
            multi,
        } => Op::Delete(pb::Delete {
            collection: collection.clone(),
            filter: filter.as_ref().map(expr_to_pb),
            single: !multi,
        }),
    };
    pb::WriteOp { op: Some(op) }
}

pub fn write_op_from_pb(op: pb::WriteOp) -> Result<WriteOp> {
    use pb::write_op::Op;
    Ok(match op.op {
        None => return err("empty write operation"),
        Some(Op::Insert(i)) => WriteOp::Insert {
            collection: i.collection,
            documents: i
                .documents
                .iter()
                .map(|d| decode_doc(d))
                .collect::<Result<_>>()?,
        },
        Some(Op::Update(u)) => WriteOp::Update {
            collection: u.collection,
            filter: u.filter.map(expr_from_pb).transpose()?,
            update: decode_doc(&u.update)?,
            multi: !u.single,
        },
        Some(Op::Delete(d)) => WriteOp::Delete {
            collection: d.collection,
            filter: d.filter.map(expr_from_pb).transpose()?,
            multi: !d.single,
        },
    })
}

pub fn write_result_to_pb(r: &WriteResult) -> pb::WriteResponse {
    pb::WriteResponse {
        inserted_ids: r
            .inserted_ids
            .iter()
            .map(|id| encode_value(id.clone()))
            .collect(),
        matched: r.matched,
        modified: r.modified,
        deleted: r.deleted,
        cascaded: r
            .cascaded
            .iter()
            .map(|c| pb::CascadeCount {
                collection: c.collection.clone(),
                deleted: c.deleted,
                nulled: c.nulled,
            })
            .collect(),
    }
}

pub fn write_result_from_pb(r: pb::WriteResponse) -> Result<WriteResult> {
    Ok(WriteResult {
        inserted_ids: r
            .inserted_ids
            .iter()
            .map(|b| decode_value(b))
            .collect::<Result<_>>()?,
        matched: r.matched,
        modified: r.modified,
        deleted: r.deleted,
        cascaded: r
            .cascaded
            .into_iter()
            .map(|c| CascadeCount {
                collection: c.collection,
                deleted: c.deleted,
                nulled: c.nulled,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::oid::ObjectId;

    #[test]
    fn schema_roundtrip() {
        let s = Schema {
            collections: vec![CollectionDef {
                name: "posts".into(),
                fields: vec![
                    FieldDef::new("title", FieldType::String).required(),
                    FieldDef::new(
                        "meta",
                        FieldType::Object(vec![FieldDef::new(
                            "tags",
                            FieldType::Array(Box::new(FieldType::String)),
                        )]),
                    ),
                ],
                relations: vec![
                    RelationDef::belongs_to("author", "users", "author_id")
                        .on_delete(OnDelete::Cascade)
                        .embed(&["name"], "author_snapshot"),
                    RelationDef::many_to_many(
                        "courses",
                        "courses",
                        Via::Junction(Junction {
                            collection: "e".into(),
                            local_key: "a".into(),
                            target_key: "b".into(),
                        }),
                    )
                    .integrity(Integrity::Loose),
                    RelationDef::has_many("x", "y", "z").references("code"),
                ],
            }],
        };
        assert_eq!(schema_from_pb(schema_to_pb(&s)).unwrap(), s);
    }

    #[test]
    fn query_roundtrip() {
        let q = Query {
            collection: "posts".into(),
            filter: Some(
                Expr::eq("author.country", "US")
                    .and(Expr::is_in("_id", [ObjectId::new(), ObjectId::new()]))
                    .and(Expr::all("comments", Expr::gt("n", 3_i64)).not())
                    .or(Expr::Raw(doc! { "x": { "$size": 2 } })),
            ),
            select: Some(vec!["title".into()]),
            include: vec![Include {
                strategy: Some(JoinStrategy::Batch),
                limit: Some(3),
                include: vec![Include::new("user")],
                ..Include::new("comments")
            }],
            sort: vec![SortKey::desc("likes")],
            skip: Some(1),
            limit: Some(10),
        };
        assert_eq!(query_from_pb(query_to_pb(&q)).unwrap(), q);
    }

    #[test]
    fn write_roundtrip() {
        let ops = vec![
            WriteOp::Insert {
                collection: "a".into(),
                documents: vec![doc! { "x": 1 }],
            },
            WriteOp::Update {
                collection: "a".into(),
                filter: Some(Expr::eq("x", 1)),
                update: doc! { "$set": { "y": 2 } },
                multi: false,
            },
            WriteOp::Delete {
                collection: "a".into(),
                filter: None,
                multi: true,
            },
        ];
        for op in ops {
            assert_eq!(write_op_from_pb(write_op_to_pb(&op)).unwrap(), op);
        }
    }

    #[test]
    fn rejects_malformed() {
        assert!(expr_from_pb(pb::Expr { kind: None }).is_err());
        let bad = pb::Expr {
            kind: Some(pb::expr::Kind::Raw(vec![1, 2, 3])),
        };
        assert!(expr_from_pb(bad).is_err());
    }
}
