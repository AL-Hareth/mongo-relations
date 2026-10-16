//! User-facing schema definitions, as stored in the catalog and sent by clients.
//!
//! These types describe *intent*. [`crate::catalog::Catalog`] resolves them
//! into join paths and physical constraints and validates them.

use serde::{Deserialize, Serialize};

pub const ID_FIELD: &str = "_id";

fn default_id() -> String {
    ID_FIELD.to_string()
}

fn is_id(s: &str) -> bool {
    s == ID_FIELD
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Schema {
    #[serde(default)]
    pub collections: Vec<CollectionDef>,
}

impl Schema {
    pub fn collection(&self, name: &str) -> Option<&CollectionDef> {
        self.collections.iter().find(|c| c.name == name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectionDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relations: Vec<RelationDef>,
}

impl CollectionDef {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            fields: vec![],
            relations: vec![],
        }
    }

    pub fn field(&self, name: &str) -> Option<&FieldDef> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub fn relation(&self, name: &str) -> Option<&RelationDef> {
        self.relations.iter().find(|r| r.name == name)
    }

    /// Resolve a dotted path through declared (possibly nested object) fields.
    /// Returns `None` if any segment is undeclared or crosses an array.
    pub fn field_at_path(&self, path: &str) -> Option<&FieldDef> {
        let mut fields = &self.fields;
        let mut segments = path.split('.').peekable();
        while let Some(seg) = segments.next() {
            let f = fields.iter().find(|f| f.name == seg)?;
            if segments.peek().is_none() {
                return Some(f);
            }
            match &f.ty {
                FieldType::Object(inner) => fields = inner,
                _ => return None,
            }
        }
        None
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldDef {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: FieldType,
    /// The field must be present.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    /// The field may hold `null` in addition to its declared type.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub nullable: bool,
    /// A unique index is maintained for this field.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unique: bool,
}

impl FieldDef {
    pub fn new(name: impl Into<String>, ty: FieldType) -> Self {
        Self {
            name: name.into(),
            ty,
            required: false,
            nullable: false,
            unique: false,
        }
    }
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }
    pub fn nullable(mut self) -> Self {
        self.nullable = true;
        self
    }
    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    ObjectId,
    String,
    /// 32-bit integer.
    Int,
    /// 64-bit integer.
    Long,
    Double,
    Decimal,
    /// Any numeric type (int, long, double, decimal).
    Number,
    Bool,
    Date,
    Binary,
    Uuid,
    /// No type constraint.
    Any,
    Array(Box<FieldType>),
    /// A nested object with its own declared fields (extras allowed).
    Object(Vec<FieldDef>),
}

impl FieldType {
    pub fn is_array(&self) -> bool {
        matches!(self, FieldType::Array(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Integrity {
    /// FK existence is checked on write and `on_delete` is enforced, inside a transaction.
    Strict,
    /// The relation only drives joins; nothing is enforced.
    Loose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnDelete {
    /// Refuse to delete a referenced document.
    Restrict,
    /// Delete referencing documents too.
    Cascade,
    /// Set a scalar FK to null, or pull the key from an array FK.
    SetNull,
    /// Do nothing (may leave dangling references).
    NoAction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelationDef {
    /// Relation name; used in includes, filter paths and as the default output alias.
    pub name: String,
    /// Target collection.
    pub target: String,
    #[serde(flatten)]
    pub kind: RelationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity: Option<Integrity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_delete: Option<OnDelete>,
    /// Keep a denormalized copy of some target fields on this document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embed: Option<EmbedDef>,
}

impl RelationDef {
    fn new(name: impl Into<String>, target: impl Into<String>, kind: RelationKind) -> Self {
        Self {
            name: name.into(),
            target: target.into(),
            kind,
            integrity: None,
            on_delete: None,
            embed: None,
        }
    }

    /// `this.foreign_key -> target._id`
    pub fn belongs_to(
        name: impl Into<String>,
        target: impl Into<String>,
        foreign_key: impl Into<String>,
    ) -> Self {
        Self::new(
            name,
            target,
            RelationKind::BelongsTo {
                foreign_key: foreign_key.into(),
                references: default_id(),
            },
        )
    }

    /// `target.foreign_key -> this._id`, at most one.
    pub fn has_one(
        name: impl Into<String>,
        target: impl Into<String>,
        foreign_key: impl Into<String>,
    ) -> Self {
        Self::new(
            name,
            target,
            RelationKind::HasOne {
                foreign_key: foreign_key.into(),
                references: default_id(),
            },
        )
    }

    /// `target.foreign_key -> this._id`, any number.
    pub fn has_many(
        name: impl Into<String>,
        target: impl Into<String>,
        foreign_key: impl Into<String>,
    ) -> Self {
        Self::new(
            name,
            target,
            RelationKind::HasMany {
                foreign_key: foreign_key.into(),
                references: default_id(),
            },
        )
    }

    pub fn many_to_many(name: impl Into<String>, target: impl Into<String>, via: Via) -> Self {
        Self::new(
            name,
            target,
            RelationKind::ManyToMany {
                via,
                references: default_id(),
            },
        )
    }

    pub fn references(mut self, field: impl Into<String>) -> Self {
        match &mut self.kind {
            RelationKind::BelongsTo { references, .. }
            | RelationKind::HasOne { references, .. }
            | RelationKind::HasMany { references, .. }
            | RelationKind::ManyToMany { references, .. } => *references = field.into(),
        }
        self
    }

    pub fn integrity(mut self, i: Integrity) -> Self {
        self.integrity = Some(i);
        self
    }

    pub fn on_delete(mut self, o: OnDelete) -> Self {
        self.on_delete = Some(o);
        self
    }

    pub fn embed(mut self, fields: &[&str], into: impl Into<String>) -> Self {
        self.embed = Some(EmbedDef {
            fields: fields.iter().map(|s| s.to_string()).collect(),
            into: into.into(),
        });
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RelationKind {
    /// The FK lives on this collection: `this.foreign_key -> target.references`.
    BelongsTo {
        foreign_key: String,
        #[serde(default = "default_id", skip_serializing_if = "is_id")]
        references: String,
    },
    /// The FK lives on the target: `target.foreign_key -> this.references`; to-one.
    HasOne {
        foreign_key: String,
        #[serde(default = "default_id", skip_serializing_if = "is_id")]
        references: String,
    },
    /// The FK lives on the target: `target.foreign_key -> this.references`; to-many.
    HasMany {
        foreign_key: String,
        #[serde(default = "default_id", skip_serializing_if = "is_id")]
        references: String,
    },
    /// Many-to-many through an id array on either side or a junction collection.
    ///
    /// `references` is the key the ids refer to: the target's key for
    /// `via.local`, this collection's key for `via.remote`. Junction rows always
    /// reference `_id` on both sides.
    ManyToMany {
        via: Via,
        #[serde(default = "default_id", skip_serializing_if = "is_id")]
        references: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    /// An array of target keys stored on this collection.
    Local(String),
    /// An array of this collection's keys stored on the target.
    Remote(String),
    /// A junction collection whose rows hold one key from each side.
    Junction(Junction),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Junction {
    pub collection: String,
    /// Junction field referencing this collection's `_id`.
    pub local_key: String,
    /// Junction field referencing the target's `_id`.
    pub target_key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbedDef {
    /// Target fields to copy.
    pub fields: Vec<String>,
    /// Field on this document that holds the copy.
    pub into: String,
}
