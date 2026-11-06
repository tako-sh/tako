use super::*;
pub(super) struct Directory;
impl Directory {
    pub(super) fn lock(&self) -> Result<()> {
        Err(crate::Error::UnsupportedPlatform)
    }
    pub(super) fn discard_stage(&self) -> Result<()> {
        Err(crate::Error::UnsupportedPlatform)
    }
    pub(super) fn publish_journal(&self, _: &[u8]) -> Result<()> {
        Err(crate::Error::UnsupportedPlatform)
    }
    pub(super) fn replace_path(&self, _: &Path, _: &[u8]) -> Result<()> {
        Err(crate::Error::UnsupportedPlatform)
    }
    pub(super) fn remove_journal(&self) -> Result<()> {
        Err(crate::Error::UnsupportedPlatform)
    }
    pub(super) fn open(_: &Path, _: bool) -> Result<Self> {
        Err(crate::Error::UnsupportedPlatform)
    }
    pub(super) fn check_path(&self, _: &Path) -> Result<()> {
        Err(crate::Error::UnsupportedPlatform)
    }
    pub(super) fn is_empty(&self) -> Result<bool> {
        Err(crate::Error::UnsupportedPlatform)
    }
    pub(super) fn read_optional(&self, _: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
        Err(crate::Error::UnsupportedPlatform)
    }
    pub(super) fn read_path(&self, _: &Path) -> Result<Zeroizing<Vec<u8>>> {
        Err(crate::Error::UnsupportedPlatform)
    }
    pub(super) fn write_path(&self, _: &Path, _: &[u8]) -> Result<()> {
        Err(crate::Error::UnsupportedPlatform)
    }
    pub(super) fn write_new(&self, _: &str, _: &[u8]) -> Result<()> {
        Err(crate::Error::UnsupportedPlatform)
    }
}
