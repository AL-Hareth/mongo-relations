//! Core of mongo-relations: schema model, catalog resolution, query IR,
//! planner and MongoDB compiler. This crate performs no I/O.

pub mod catalog;
pub mod compile;
pub mod ejson;
pub mod error;
pub mod json_schema;
pub mod path;
pub mod plan;
pub mod query;
pub mod schema;
pub mod validate;
pub mod write;

pub use bson;
pub use catalog::Catalog;
pub use error::{Error, Result, Violation};
