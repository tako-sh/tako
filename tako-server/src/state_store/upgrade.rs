use super::*;
use tako_core::UpgradeMode;

pub(crate) fn promotion_ready(path: &Path) -> Result<bool, StateStoreError> {
    inspect(path, |r| {
        Ok(
            get::<UpgradeMode>(r, ("mode", "", ""))? == Some(UpgradeMode::Normal)
                && r.get(("upgrade_lock", "", "")).map_err(storage)?.is_none(),
        )
    })
}
impl StateStore {
    pub fn set_server_mode(&self, mode: UpgradeMode) -> Result<(), StateStoreError> {
        self.write(|r| put(r, ("mode", "", ""), &mode))
    }
    pub fn server_mode(&self) -> Result<UpgradeMode, StateStoreError> {
        self.read(|r| get(r, ("mode", "", ""))?.ok_or_else(|| storage("missing server mode")))
    }
    pub(crate) const UPGRADE_LOCK_STALE_SECS: i64 = 600;
    pub fn try_acquire_upgrade_lock(&self, owner: &str) -> Result<bool, StateStoreError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(storage)?
            .as_secs() as i64;
        self.write(|r| {
            if let Some((current, time)) = get::<(String, i64)>(r, ("upgrade_lock", "", ""))? {
                if current != owner && now.saturating_sub(time) <= Self::UPGRADE_LOCK_STALE_SECS {
                    return Ok(false);
                }
            }
            put(r, ("upgrade_lock", "", ""), &(owner, now))?;
            Ok(true)
        })
    }
    pub fn release_upgrade_lock(&self, owner: &str) -> Result<bool, StateStoreError> {
        self.write(|r| {
            if !get::<(String, i64)>(r, ("upgrade_lock", "", ""))?.is_some_and(|(v, _)| v == owner)
            {
                return Ok(false);
            }
            remove(r, ("upgrade_lock", "", ""))?;
            Ok(true)
        })
    }
    pub fn upgrade_lock_owner(&self) -> Result<Option<String>, StateStoreError> {
        self.read(|r| Ok(get::<(String, i64)>(r, ("upgrade_lock", "", ""))?.map(|(v, _)| v)))
    }
}
