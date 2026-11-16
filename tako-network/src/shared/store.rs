use super::{SharedRecord, SharedSnapshot, SharedStore};
use crate::api::RuntimeResult;

impl SharedStore for SharedSnapshot {
    fn shared_snapshot(&mut self) -> RuntimeResult<SharedSnapshot> {
        self.validate()?;
        Ok(self.clone())
    }

    fn append_record(&mut self, record: &SharedRecord) -> RuntimeResult<()> {
        let mut addition = Self::empty(self.network.clone());
        addition.records.push(record.clone());
        self.merge(&addition)
    }
}
