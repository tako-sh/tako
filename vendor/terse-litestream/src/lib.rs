#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

mod database;
mod replica;
mod store;
mod wal;

pub use database::{CheckpointMode, Database, Options, SyncResult};
pub use replica::{compact, replicate, restore, restore_plan, restore_with};
pub use store::{Durability, FileStore, ReplicaStore, Segment, SegmentWriter};
