#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

mod replica;
mod store;

pub use replica::{compact, replicate, restore, restore_plan, restore_with};
pub use store::{Durability, FileStore, ReplicaStore, Segment, SegmentWriter};
