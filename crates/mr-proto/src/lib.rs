//! gRPC definitions for mongo-relations and lossless conversions between the
//! protobuf messages and the `mr-core` IR.

pub mod pb {
    #![allow(clippy::all)]
    tonic::include_proto!("mongo_relations.v1");
}

mod convert;

pub use convert::*;
