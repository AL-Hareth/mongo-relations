//! Resolved, validated schema.
//!
//! Every relation is compiled into a [`JoinPath`] (equality hops between
//! collections) and every physical foreign key into a [`Constraint`]. Two
//! relation declarations that describe the same physical FK (e.g. a
//! `belongs_to` and its `has_many` inverse) are merged into a single
//! constraint, and their integrity settings must agree.

use std::collections::{BTreeMap, HashSet};

use bson::{Document, doc};
use indexmap::IndexMap;

use crate::error::{Error, Result, schema_err};
use crate::schema::*;

/// One equality join step: `from.from_field == to.to_field`.
/// Either side may be an array, in which case MongoDB's array equality
/// semantics apply (an element matches).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop {
    pub from: String,
    pub from_field: String,
    pub from_array: bool,
    pub to: String,
    pub to_field: String,
    pub to_array: bool,
}

pub type JoinPath = Vec<Hop>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cardinality {
    One,
    Many,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKindTag {
    BelongsTo,
    HasOne,
    HasMany,
    ManyToMany,
}

/// A resolved relation, usable by the planner without knowing its kind.
#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    pub name: String,
    pub source: String,
    pub target: String,
    pub kind: RelationKindTag,
    pub cardinality: Cardinality,
    pub path: JoinPath,
    /// Index into [`Catalog::embeds`].
    pub embed: Option<usize>,
    /// Index into [`Catalog::constraints`] for the FK the first hop starts
    /// from, when the source collection owns it (belongs_to / local array).
    pub owned_constraint: Option<usize>,
}

/// A physical foreign key: `owner.field` holds key(s) of `target.target_field`.
#[derive(Debug, Clone, PartialEq)]
pub struct Constraint {
    pub owner: String,
    pub field: String,
    /// `field` holds an array of keys.
    pub array: bool,
    pub target: String,
    pub target_field: String,
    pub integrity: Integrity,
    pub on_delete: OnDelete,
    /// `collection.relation` names that declared this FK.
    pub declared_by: Vec<String>,
}

impl Constraint {
    pub fn is_strict(&self) -> bool {
        self.integrity == Integrity::Strict
    }
}

/// A denormalized copy of target fields kept on the owning document.
#[derive(Debug, Clone, PartialEq)]
pub struct Embed {
    pub relation: String,
    pub owner: String,
    /// FK field on `owner`.
    pub field: String,
    pub array: bool,
    pub target: String,
    pub target_field: String,
    pub fields: Vec<String>,
    pub into: String,
}

impl Embed {
    /// Build the snapshot stored on the owner from a target document.
    ///
    /// Snapshots always carry the target's `_id`, so copies can be found
    /// (and removed) when the target is deleted, even though delete events
    /// only carry the `_id`.
    pub fn snapshot(&self, target_doc: &Document) -> Document {
        let mut out = Document::new();
        if let Some(id) = target_doc.get(ID_FIELD) {
            out.insert(ID_FIELD, id.clone());
        }
        if self.array && self.target_field != ID_FIELD {
            if let Some(k) = target_doc.get(&self.target_field) {
                out.insert(self.target_field.clone(), k.clone());
            }
        }
        for f in &self.fields {
            if let Some(v) = crate::path::get_path(target_doc, f) {
                out.insert(f.clone(), v.clone());
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexSpec {
    pub name: String,
    pub keys: Document,
    pub unique: bool,
    /// Only index documents matching this filter (`partialFilterExpression`).
    pub partial: Option<Document>,
}

#[derive(Debug, Clone)]
pub struct CollectionInfo {
    pub def: CollectionDef,
    pub relations: IndexMap<String, Relation>,
    /// Constraints owned by (stored on) this collection.
    pub outbound: Vec<usize>,
    /// Constraints that reference this collection.
    pub inbound: Vec<usize>,
    /// Embeds stored on this collection.
    pub embeds_out: Vec<usize>,
    /// Embeds that copy fields from this collection.
    pub embeds_in: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct Catalog {
    version: u64,
    schema: Schema,
    collections: IndexMap<String, CollectionInfo>,
    constraints: Vec<Constraint>,
    embeds: Vec<Embed>,
}

const RESERVED_PREFIX: &str = "_mr_";

/// `(owner collection, field)` of a physical FK.
type FkRef = (String, String);

fn check_name(kind: &str, name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(schema_err!("{kind} name must not be empty"));
    }
    if name.starts_with('$') || name.contains('.') || name.contains('\0') {
        return Err(schema_err!(
            "{kind} name `{name}` must not start with `$` or contain `.`"
        ));
    }
    Ok(())
}

fn check_path(kind: &str, path: &str) -> Result<()> {
    for seg in path.split('.') {
        check_name(kind, seg)?;
    }
    Ok(())
}

#[derive(Debug)]
struct Draft {
    owner: String,
    field: String,
    array: bool,
    target: String,
    target_field: String,
    integrity: Option<Integrity>,
    on_delete: Option<OnDelete>,
    default_on_delete: OnDelete,
    declared_by: Vec<String>,
}

impl Catalog {
    pub fn empty() -> Self {
        Self::new(Schema::default(), 0).expect("empty schema is valid")
    }

    pub fn new(schema: Schema, version: u64) -> Result<Self> {
        let mut collections: IndexMap<String, CollectionInfo> = IndexMap::new();

        // Pass 1: collections and fields.
        for c in &schema.collections {
            check_name("collection", &c.name)?;
            if c.name.starts_with(RESERVED_PREFIX) || c.name.starts_with("system.") {
                return Err(schema_err!("collection name `{}` is reserved", c.name));
            }
            if collections.contains_key(&c.name) {
                return Err(schema_err!("collection `{}` is declared twice", c.name));
            }
            validate_fields(&c.name, "", &c.fields)?;
            let mut rel_names = HashSet::new();
            for r in &c.relations {
                check_name("relation", &r.name)?;
                if !rel_names.insert(r.name.as_str()) {
                    return Err(schema_err!(
                        "relation `{}.{}` is declared twice",
                        c.name,
                        r.name
                    ));
                }
                if c.field(&r.name).is_some() {
                    return Err(schema_err!(
                        "relation `{}.{}` has the same name as a declared field",
                        c.name,
                        r.name
                    ));
                }
            }
            collections.insert(
                c.name.clone(),
                CollectionInfo {
                    def: c.clone(),
                    relations: IndexMap::new(),
                    outbound: vec![],
                    inbound: vec![],
                    embeds_out: vec![],
                    embeds_in: vec![],
                },
            );
        }

        // Pass 2: relations -> join paths, constraint drafts, embeds.
        let mut drafts: BTreeMap<(String, String), Draft> = BTreeMap::new();
        let mut embeds: Vec<Embed> = vec![];
        // (source collection, relation, owned FK as (owner, field))
        let mut relations: Vec<(String, Relation, Option<FkRef>)> = vec![];

        for c in &schema.collections {
            for r in &c.relations {
                let qual = format!("{}.{}", c.name, r.name);
                let target = schema.collection(&r.target).ok_or_else(|| {
                    schema_err!(
                        "relation `{qual}` targets unknown collection `{}`",
                        r.target
                    )
                })?;
                if r.integrity == Some(Integrity::Loose) && r.on_delete.is_some() {
                    return Err(schema_err!(
                        "relation `{qual}`: `on_delete` requires strict integrity"
                    ));
                }

                let mut add = |d: Draft| -> Result<()> { merge_draft(&mut drafts, d) };
                let draft =
                    |owner: &str, field: &str, array: bool, tgt: &str, tf: &str, def| Draft {
                        owner: owner.into(),
                        field: field.into(),
                        array,
                        target: tgt.into(),
                        target_field: tf.into(),
                        integrity: r.integrity,
                        on_delete: r.on_delete,
                        default_on_delete: def,
                        declared_by: vec![qual.clone()],
                    };

                let (kind, cardinality, path, owned): (_, _, JoinPath, Option<FkRef>) = match &r
                    .kind
                {
                    RelationKind::BelongsTo {
                        foreign_key,
                        references,
                    } => {
                        check_path("foreign key", foreign_key)?;
                        check_key_field(target, references, &qual)?;
                        check_fk_shape(c, foreign_key, false, &qual)?;
                        add(draft(
                            &c.name,
                            foreign_key,
                            false,
                            &target.name,
                            references,
                            OnDelete::Restrict,
                        ))?;
                        (
                            RelationKindTag::BelongsTo,
                            Cardinality::One,
                            vec![Hop {
                                from: c.name.clone(),
                                from_field: foreign_key.clone(),
                                from_array: false,
                                to: target.name.clone(),
                                to_field: references.clone(),
                                to_array: false,
                            }],
                            Some((c.name.clone(), foreign_key.clone())),
                        )
                    }
                    RelationKind::HasOne {
                        foreign_key,
                        references,
                    }
                    | RelationKind::HasMany {
                        foreign_key,
                        references,
                    } => {
                        check_path("foreign key", foreign_key)?;
                        check_key_field(c, references, &qual)?;
                        check_fk_shape(target, foreign_key, false, &qual)?;
                        add(draft(
                            &target.name,
                            foreign_key,
                            false,
                            &c.name,
                            references,
                            OnDelete::Restrict,
                        ))?;
                        let (k, card) = if matches!(r.kind, RelationKind::HasOne { .. }) {
                            (RelationKindTag::HasOne, Cardinality::One)
                        } else {
                            (RelationKindTag::HasMany, Cardinality::Many)
                        };
                        (
                            k,
                            card,
                            vec![Hop {
                                from: c.name.clone(),
                                from_field: references.clone(),
                                from_array: false,
                                to: target.name.clone(),
                                to_field: foreign_key.clone(),
                                to_array: false,
                            }],
                            None,
                        )
                    }
                    RelationKind::ManyToMany { via, references } => match via {
                        Via::Local(field) => {
                            check_path("foreign key", field)?;
                            check_key_field(target, references, &qual)?;
                            check_fk_shape(c, field, true, &qual)?;
                            add(draft(
                                &c.name,
                                field,
                                true,
                                &target.name,
                                references,
                                OnDelete::Restrict,
                            ))?;
                            (
                                RelationKindTag::ManyToMany,
                                Cardinality::Many,
                                vec![Hop {
                                    from: c.name.clone(),
                                    from_field: field.clone(),
                                    from_array: true,
                                    to: target.name.clone(),
                                    to_field: references.clone(),
                                    to_array: false,
                                }],
                                Some((c.name.clone(), field.clone())),
                            )
                        }
                        Via::Remote(field) => {
                            check_path("foreign key", field)?;
                            check_key_field(c, references, &qual)?;
                            check_fk_shape(target, field, true, &qual)?;
                            add(draft(
                                &target.name,
                                field,
                                true,
                                &c.name,
                                references,
                                OnDelete::Restrict,
                            ))?;
                            (
                                RelationKindTag::ManyToMany,
                                Cardinality::Many,
                                vec![Hop {
                                    from: c.name.clone(),
                                    from_field: references.clone(),
                                    from_array: false,
                                    to: target.name.clone(),
                                    to_field: field.clone(),
                                    to_array: true,
                                }],
                                None,
                            )
                        }
                        Via::Junction(j) => {
                            if references != ID_FIELD {
                                return Err(schema_err!(
                                    "relation `{qual}`: junction relations always reference `_id`"
                                ));
                            }
                            let jc = schema.collection(&j.collection).ok_or_else(|| {
                                schema_err!(
                                    "relation `{qual}` uses unknown junction collection `{}`",
                                    j.collection
                                )
                            })?;
                            check_path("junction key", &j.local_key)?;
                            check_path("junction key", &j.target_key)?;
                            if j.local_key == j.target_key {
                                return Err(schema_err!(
                                    "relation `{qual}`: junction keys must differ"
                                ));
                            }
                            check_fk_shape(jc, &j.local_key, false, &qual)?;
                            check_fk_shape(jc, &j.target_key, false, &qual)?;
                            add(draft(
                                &jc.name,
                                &j.local_key,
                                false,
                                &c.name,
                                ID_FIELD,
                                OnDelete::Cascade,
                            ))?;
                            add(draft(
                                &jc.name,
                                &j.target_key,
                                false,
                                &target.name,
                                ID_FIELD,
                                OnDelete::Cascade,
                            ))?;
                            (
                                RelationKindTag::ManyToMany,
                                Cardinality::Many,
                                vec![
                                    Hop {
                                        from: c.name.clone(),
                                        from_field: ID_FIELD.into(),
                                        from_array: false,
                                        to: jc.name.clone(),
                                        to_field: j.local_key.clone(),
                                        to_array: false,
                                    },
                                    Hop {
                                        from: jc.name.clone(),
                                        from_field: j.target_key.clone(),
                                        from_array: false,
                                        to: target.name.clone(),
                                        to_field: ID_FIELD.into(),
                                        to_array: false,
                                    },
                                ],
                                None,
                            )
                        }
                    },
                };

                let embed = match &r.embed {
                    None => None,
                    Some(e) => {
                        let hop = &path[0];
                        let allowed = matches!(kind, RelationKindTag::BelongsTo)
                            || matches!(
                                &r.kind,
                                RelationKind::ManyToMany {
                                    via: Via::Local(_),
                                    ..
                                }
                            );
                        if !allowed {
                            return Err(schema_err!(
                                "relation `{qual}`: embed is only supported on belongs_to and \
                                 many_to_many with `via.local`"
                            ));
                        }
                        check_name("embed target", &e.into)?;
                        if e.fields.is_empty() {
                            return Err(schema_err!("relation `{qual}`: embed needs fields"));
                        }
                        let mut seen = HashSet::new();
                        for f in &e.fields {
                            check_path("embed field", f)?;
                            if !seen.insert(f) {
                                return Err(schema_err!(
                                    "relation `{qual}`: embed field `{f}` listed twice"
                                ));
                            }
                        }
                        if e.into == hop.from_field
                            || c.field(&e.into).is_some()
                            || c.relation(&e.into).is_some()
                        {
                            return Err(schema_err!(
                                "relation `{qual}`: embed target `{}` collides with a field or \
                                 relation",
                                e.into
                            ));
                        }
                        if embeds.iter().any(|x| x.owner == c.name && x.into == e.into) {
                            return Err(schema_err!(
                                "relation `{qual}`: embed target `{}` is already used",
                                e.into
                            ));
                        }
                        embeds.push(Embed {
                            relation: r.name.clone(),
                            owner: c.name.clone(),
                            field: hop.from_field.clone(),
                            array: hop.from_array,
                            target: target.name.clone(),
                            target_field: hop.to_field.clone(),
                            fields: e.fields.clone(),
                            into: e.into.clone(),
                        });
                        Some(embeds.len() - 1)
                    }
                };

                relations.push((
                    c.name.clone(),
                    Relation {
                        name: r.name.clone(),
                        source: c.name.clone(),
                        target: target.name.clone(),
                        kind,
                        cardinality,
                        path,
                        embed,
                        owned_constraint: None,
                    },
                    owned,
                ));
            }
        }

        // Embeds must not copy another embed's snapshot: chained copies would
        // make the sync worker's own writes trigger further syncs.
        for e in &embeds {
            for other in embeds.iter().filter(|o| o.owner == e.target) {
                if e.fields
                    .iter()
                    .any(|f| crate::path::paths_overlap(f, &other.into))
                {
                    return Err(schema_err!(
                        "relation `{}.{}`: embed copies `{}.{}`, which is itself an embed",
                        e.owner,
                        e.relation,
                        e.target,
                        other.into
                    ));
                }
            }
        }

        // Finalize constraints.
        let mut constraints = Vec::with_capacity(drafts.len());
        for d in drafts.into_values() {
            let integrity = d.integrity.unwrap_or(Integrity::Strict);
            let on_delete = d.on_delete.unwrap_or(d.default_on_delete);
            if on_delete == OnDelete::SetNull && !d.array {
                let owner = schema.collection(&d.owner).expect("validated");
                if owner
                    .field_at_path(&d.field)
                    .is_some_and(|f| f.required && !f.nullable)
                {
                    return Err(schema_err!(
                        "FK `{}.{}` is required and not nullable, so `on_delete: set_null` is \
                         impossible (declared by {})",
                        d.owner,
                        d.field,
                        d.declared_by.join(", ")
                    ));
                }
            }
            constraints.push(Constraint {
                owner: d.owner,
                field: d.field,
                array: d.array,
                target: d.target,
                target_field: d.target_field,
                integrity,
                on_delete,
                declared_by: d.declared_by,
            });
        }
        for (i, c) in constraints.iter().enumerate() {
            collections[&c.owner].outbound.push(i);
            collections[&c.target].inbound.push(i);
        }
        for (i, e) in embeds.iter().enumerate() {
            collections[&e.owner].embeds_out.push(i);
            collections[&e.target].embeds_in.push(i);
        }
        for (coll, mut rel, owned) in relations {
            rel.owned_constraint = owned.and_then(|(o, f)| {
                constraints
                    .iter()
                    .position(|c| c.owner == o && c.field == f)
            });
            collections[&coll].relations.insert(rel.name.clone(), rel);
        }

        Ok(Self {
            version,
            schema,
            collections,
            constraints,
            embeds,
        })
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn collections(&self) -> impl Iterator<Item = &CollectionInfo> {
        self.collections.values()
    }

    pub fn collection(&self, name: &str) -> Result<&CollectionInfo> {
        self.collections
            .get(name)
            .ok_or_else(|| Error::UnknownCollection(name.to_string()))
    }

    pub fn relation(&self, collection: &str, name: &str) -> Result<&Relation> {
        self.collection(collection)?
            .relations
            .get(name)
            .ok_or_else(|| Error::UnknownRelation {
                collection: collection.to_string(),
                relation: name.to_string(),
            })
    }

    pub fn constraints(&self) -> &[Constraint] {
        &self.constraints
    }

    pub fn constraint(&self, i: usize) -> &Constraint {
        &self.constraints[i]
    }

    pub fn embeds(&self) -> &[Embed] {
        &self.embeds
    }

    pub fn embed(&self, i: usize) -> &Embed {
        &self.embeds[i]
    }

    pub fn inbound(&self, collection: &str) -> impl Iterator<Item = &Constraint> {
        self.collections
            .get(collection)
            .into_iter()
            .flat_map(|c| c.inbound.iter().map(|&i| &self.constraints[i]))
    }

    pub fn outbound(&self, collection: &str) -> impl Iterator<Item = &Constraint> {
        self.collections
            .get(collection)
            .into_iter()
            .flat_map(|c| c.outbound.iter().map(|&i| &self.constraints[i]))
    }

    /// Embeds stored on `collection`.
    pub fn embeds_on(&self, collection: &str) -> impl Iterator<Item = &Embed> {
        self.collections
            .get(collection)
            .into_iter()
            .flat_map(|c| c.embeds_out.iter().map(|&i| &self.embeds[i]))
    }

    /// Embeds copying fields from `collection`.
    pub fn embeds_from(&self, collection: &str) -> impl Iterator<Item = &Embed> {
        self.collections
            .get(collection)
            .into_iter()
            .flat_map(|c| c.embeds_in.iter().map(|&i| &self.embeds[i]))
    }

    /// The `$jsonSchema` validator for a collection's declared fields.
    pub fn json_schema(&self, collection: &str) -> Result<Document> {
        Ok(crate::json_schema::collection_schema(
            &self.collection(collection)?.def,
        ))
    }

    /// Indexes the layer maintains on a collection: unique declared fields and
    /// every FK / join field stored on it.
    pub fn indexes(&self, collection: &str) -> Result<Vec<IndexSpec>> {
        let info = self.collection(collection)?;
        let mut out: Vec<IndexSpec> = vec![];
        collect_unique_indexes(&info.def.fields, "", &mut out);
        let mut push = |field: &str| {
            if field == ID_FIELD || out.iter().any(|i| i.keys.contains_key(field)) {
                return;
            }
            out.push(IndexSpec {
                name: format!("mr_fk_{field}"),
                keys: doc! { field: 1 },
                unique: false,
                partial: None,
            });
        };
        for c in self.outbound(collection) {
            push(&c.field);
        }
        for e in self.embeds_on(collection) {
            push(&format!("{}.{ID_FIELD}", e.into));
        }
        for coll in self.collections.values() {
            for rel in coll.relations.values() {
                for hop in &rel.path {
                    if hop.to == collection {
                        push(&hop.to_field);
                    }
                }
            }
        }
        Ok(out)
    }
}

fn collect_unique_indexes(fields: &[FieldDef], prefix: &str, out: &mut Vec<IndexSpec>) {
    for f in fields {
        let path = if prefix.is_empty() {
            f.name.clone()
        } else {
            format!("{prefix}.{}", f.name)
        };
        if f.unique {
            // Like SQL UNIQUE, any number of documents may lack a value:
            // a plain unique index would treat every missing field as a
            // duplicate `null`, so optional fields get a partial index over
            // documents that hold a value of the declared type.
            let partial = (!f.required || f.nullable).then(|| {
                let cond = match crate::json_schema::bson_types(&f.ty) {
                    Some(types) => doc! { "$type": types },
                    None => doc! { "$exists": true },
                };
                doc! { path.clone(): cond }
            });
            out.push(IndexSpec {
                name: format!("mr_uniq_{path}"),
                keys: doc! { path.clone(): 1 },
                unique: true,
                partial,
            });
        }
        if let FieldType::Object(inner) = &f.ty {
            collect_unique_indexes(inner, &path, out);
        }
    }
}

fn validate_fields(coll: &str, prefix: &str, fields: &[FieldDef]) -> Result<()> {
    let mut seen = HashSet::new();
    for f in fields {
        check_name("field", &f.name)?;
        let path = if prefix.is_empty() {
            f.name.clone()
        } else {
            format!("{prefix}.{}", f.name)
        };
        if !seen.insert(f.name.as_str()) {
            return Err(schema_err!("field `{coll}.{path}` is declared twice"));
        }
        if f.name == ID_FIELD && prefix.is_empty() && f.nullable {
            return Err(schema_err!("`{coll}._id` cannot be nullable"));
        }
        validate_type(coll, &path, &f.ty)?;
    }
    Ok(())
}

fn validate_type(coll: &str, path: &str, ty: &FieldType) -> Result<()> {
    match ty {
        FieldType::Object(inner) => validate_fields(coll, path, inner),
        FieldType::Array(inner) => validate_type(coll, path, inner),
        _ => Ok(()),
    }
}

/// A key referenced by a FK must be `_id` or a declared unique top-level field.
fn check_key_field(c: &CollectionDef, field: &str, qual: &str) -> Result<()> {
    if field == ID_FIELD {
        return Ok(());
    }
    match c.field(field) {
        Some(f) if f.unique && !f.ty.is_array() => Ok(()),
        _ => Err(schema_err!(
            "relation `{qual}` references `{}.{field}`, which must be `_id` or a declared unique \
             scalar field",
            c.name
        )),
    }
}

/// If a FK field is declared, its shape must match the relation (array or scalar).
fn check_fk_shape(c: &CollectionDef, field: &str, array: bool, qual: &str) -> Result<()> {
    if field == ID_FIELD {
        return Err(schema_err!(
            "relation `{qual}`: `_id` cannot be a foreign key"
        ));
    }
    if let Some(f) = c.field_at_path(field) {
        if f.ty.is_array() != array {
            return Err(schema_err!(
                "relation `{qual}`: field `{}.{field}` must be {}",
                c.name,
                if array { "an array" } else { "a scalar" }
            ));
        }
    }
    Ok(())
}

fn merge_draft(drafts: &mut BTreeMap<(String, String), Draft>, d: Draft) -> Result<()> {
    let key = (d.owner.clone(), d.field.clone());
    let Some(existing) = drafts.get_mut(&key) else {
        drafts.insert(key, d);
        return Ok(());
    };
    let who = || {
        format!(
            "{} and {}",
            existing.declared_by.join(", "),
            d.declared_by.join(", ")
        )
    };
    if existing.target != d.target || existing.target_field != d.target_field {
        return Err(schema_err!(
            "FK `{}.{}` is declared to reference both `{}.{}` and `{}.{}` ({})",
            d.owner,
            d.field,
            existing.target,
            existing.target_field,
            d.target,
            d.target_field,
            who()
        ));
    }
    if existing.array != d.array {
        return Err(schema_err!(
            "FK `{}.{}` is declared both as an array and as a scalar ({})",
            d.owner,
            d.field,
            who()
        ));
    }
    match (existing.integrity, d.integrity) {
        (Some(a), Some(b)) if a != b => {
            return Err(schema_err!(
                "FK `{}.{}` has conflicting integrity {a:?} vs {b:?} ({})",
                d.owner,
                d.field,
                who()
            ));
        }
        (None, Some(b)) => existing.integrity = Some(b),
        _ => {}
    }
    match (existing.on_delete, d.on_delete) {
        (Some(a), Some(b)) if a != b => {
            return Err(schema_err!(
                "FK `{}.{}` has conflicting on_delete {a:?} vs {b:?} ({})",
                d.owner,
                d.field,
                who()
            ));
        }
        (None, Some(b)) => existing.on_delete = Some(b),
        _ => {}
    }
    if d.default_on_delete == OnDelete::Cascade {
        existing.default_on_delete = OnDelete::Cascade;
    }
    existing.declared_by.extend(d.declared_by);
    if existing.integrity == Some(Integrity::Loose) && existing.on_delete.is_some() {
        return Err(schema_err!(
            "FK `{}.{}`: `on_delete` requires strict integrity ({})",
            existing.owner,
            existing.field,
            existing.declared_by.join(", ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn blog() -> Schema {
        Schema {
            collections: vec![
                CollectionDef {
                    name: "users".into(),
                    fields: vec![
                        FieldDef::new("email", FieldType::String)
                            .required()
                            .unique(),
                        FieldDef::new("name", FieldType::String),
                    ],
                    relations: vec![
                        RelationDef::has_many("posts", "posts", "author_id")
                            .on_delete(OnDelete::Cascade),
                        RelationDef::has_one("profile", "profiles", "user_id"),
                    ],
                },
                CollectionDef {
                    name: "posts".into(),
                    fields: vec![FieldDef::new("title", FieldType::String).required()],
                    relations: vec![
                        RelationDef::belongs_to("author", "users", "author_id")
                            .embed(&["name"], "author_snapshot"),
                        RelationDef::many_to_many("tags", "tags", Via::Local("tag_ids".into()))
                            .on_delete(OnDelete::SetNull),
                    ],
                },
                CollectionDef::new("profiles"),
                CollectionDef {
                    name: "tags".into(),
                    fields: vec![],
                    relations: vec![RelationDef::many_to_many(
                        "posts",
                        "posts",
                        Via::Remote("tag_ids".into()),
                    )],
                },
            ],
        }
    }

    #[test]
    fn merges_both_sides_of_a_relation() {
        let cat = Catalog::new(blog(), 1).unwrap();
        let fk: Vec<_> = cat
            .constraints()
            .iter()
            .filter(|c| c.owner == "posts" && c.field == "author_id")
            .collect();
        assert_eq!(fk.len(), 1);
        assert_eq!(fk[0].on_delete, OnDelete::Cascade);
        assert_eq!(fk[0].integrity, Integrity::Strict);
        assert_eq!(fk[0].declared_by, vec!["users.posts", "posts.author"]);

        let tags: Vec<_> = cat
            .constraints()
            .iter()
            .filter(|c| c.field == "tag_ids")
            .collect();
        assert_eq!(tags.len(), 1);
        assert!(tags[0].array);
        assert_eq!(tags[0].on_delete, OnDelete::SetNull);
    }

    #[test]
    fn rejects_conflicting_sides() {
        let mut s = blog();
        s.collections[1].relations[0].on_delete = Some(OnDelete::Restrict);
        let err = Catalog::new(s, 1).unwrap_err();
        assert!(err.to_string().contains("conflicting on_delete"), "{err}");
    }

    #[test]
    fn rejects_unknown_target() {
        let mut s = blog();
        s.collections[1].relations[0].target = "nope".into();
        assert!(Catalog::new(s, 1).is_err());
    }

    #[test]
    fn rejects_set_null_on_required_fk() {
        let mut s = blog();
        s.collections[1]
            .fields
            .push(FieldDef::new("author_id", FieldType::ObjectId).required());
        s.collections[0].relations[0].on_delete = Some(OnDelete::SetNull);
        let err = Catalog::new(s, 1).unwrap_err();
        assert!(err.to_string().contains("set_null"), "{err}");
    }

    #[test]
    fn rejects_fk_shape_mismatch() {
        let mut s = blog();
        s.collections[1]
            .fields
            .push(FieldDef::new("tag_ids", FieldType::ObjectId));
        assert!(Catalog::new(s, 1).is_err());
    }

    #[test]
    fn junction_paths_and_cascade_default() {
        let s = Schema {
            collections: vec![
                CollectionDef {
                    name: "students".into(),
                    fields: vec![],
                    relations: vec![RelationDef::many_to_many(
                        "courses",
                        "courses",
                        Via::Junction(Junction {
                            collection: "enrollments".into(),
                            local_key: "student_id".into(),
                            target_key: "course_id".into(),
                        }),
                    )],
                },
                CollectionDef::new("courses"),
                CollectionDef::new("enrollments"),
            ],
        };
        let cat = Catalog::new(s, 1).unwrap();
        let rel = cat.relation("students", "courses").unwrap();
        assert_eq!(rel.path.len(), 2);
        assert_eq!(rel.path[0].to, "enrollments");
        assert_eq!(rel.path[1].to, "courses");
        assert!(
            cat.constraints()
                .iter()
                .all(|c| c.on_delete == OnDelete::Cascade)
        );
        let idx = cat.indexes("enrollments").unwrap();
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn schema_json_roundtrip() {
        let s = blog();
        let json = serde_json::to_string_pretty(&s).unwrap();
        let back: Schema = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
        assert!(json.contains("\"kind\": \"belongs_to\""));
        assert!(json.contains("\"local\": \"tag_ids\""));
    }

    #[test]
    fn optional_unique_fields_get_partial_indexes() {
        let mut s = blog();
        s.collections[0]
            .fields
            .push(FieldDef::new("handle", FieldType::String).unique());
        let cat = Catalog::new(s, 1).unwrap();
        let idx = cat.indexes("users").unwrap();
        let email = idx.iter().find(|i| i.name == "mr_uniq_email").unwrap();
        assert_eq!(
            email.partial, None,
            "required fields need no partial filter"
        );
        let handle = idx.iter().find(|i| i.name == "mr_uniq_handle").unwrap();
        assert_eq!(
            handle.partial,
            Some(doc! { "handle": { "$type": ["string"] } })
        );
    }

    #[test]
    fn indexes_cover_unique_and_fk_fields() {
        let cat = Catalog::new(blog(), 1).unwrap();
        let users = cat.indexes("users").unwrap();
        assert!(
            users
                .iter()
                .any(|i| i.unique && i.keys.contains_key("email"))
        );
        let posts = cat.indexes("posts").unwrap();
        assert!(posts.iter().any(|i| i.keys.contains_key("author_id")));
        assert!(posts.iter().any(|i| i.keys.contains_key("tag_ids")));
        let profiles = cat.indexes("profiles").unwrap();
        assert!(profiles.iter().any(|i| i.keys.contains_key("user_id")));
    }
}
