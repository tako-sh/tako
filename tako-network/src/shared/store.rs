use super::{SecretGrant, SecretRecord, SharedSnapshot, SharedStore};
use crate::api::RuntimeResult;

impl SharedStore for SharedSnapshot {
    fn shared_snapshot(&mut self) -> RuntimeResult<SharedSnapshot> {
        self.validate()?;
        Ok(self.clone())
    }

    fn append_secret(&mut self, record: &SecretRecord) -> RuntimeResult<()> {
        let mut addition = Self::empty(self.network.clone());
        addition.secrets.push(record.clone());
        self.merge(&addition)
    }

    fn append_secret_grant(&mut self, grant: &SecretGrant) -> RuntimeResult<()> {
        let mut addition = Self::empty(self.network.clone());
        addition.grants.push(grant.clone());
        self.merge(&addition)
    }
}
