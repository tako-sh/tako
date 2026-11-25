//! Turso calls this storage after writing a framed transaction and before it
//! publishes the transaction to any SQL connection. A failed Raft proposal
//! permanently fences the private database, including after an ambiguous reply.
use super::image::{Layout, PAGE};
use parking_lot::Mutex;
use std::{fmt, fs, io, path::PathBuf, sync::Arc};
use tako_raft::Writer;
use turso_core::{
    CheckpointResult, Completion, File, LimboError, Result,
    io::FileSyncType,
    mvcc::{
        database::{LogRecord, RowID, RowVersion},
        persistent_storage::{
            DurableStorage, LogicalLogTruncateOutcome, Storage,
            logical_log::{LogHeader, OnSerializationComplete},
        },
    },
    storage::sqlite3_ondisk::DatabaseHeader,
};

pub(super) struct ReplicatedLog {
    inner: Storage,
    dir: PathBuf,
    state: Mutex<State>,
}
struct State {
    writer: Writer,
    layout: Layout,
    refresh_base: bool,
    pending_end: Option<u64>,
    published: bool,
}
impl fmt::Debug for ReplicatedLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReplicatedLog")
            .field("directory", &self.dir)
            .finish_non_exhaustive()
    }
}
impl ReplicatedLog {
    pub fn new(inner: Storage, dir: PathBuf, writer: Writer, layout: Option<Layout>) -> Self {
        Self {
            inner,
            dir,
            state: Mutex::new(State {
                writer,
                layout: layout.unwrap_or_default(),
                refresh_base: layout.is_none(),
                pending_end: None,
                published: false,
            }),
        }
    }
    pub fn usable(&self) -> bool {
        self.state.lock().writer.revision_hint().is_ok()
    }
    pub fn barrier(&self) -> io::Result<()> {
        let mut state = self.state.lock();
        let result = state.writer.check_current();
        if result.is_err() {
            state.writer.fence();
        }
        result
    }

    pub fn revision(&self) -> io::Result<u64> {
        self.state.lock().writer.revision_hint()
    }
    pub fn confirm(&self, before: u64) -> io::Result<()> {
        if self.revision()? == before {
            self.barrier()?;
        }
        Ok(())
    }
    pub fn fence(&self) {
        self.state.lock().writer.fence();
    }
    fn publish(&self, state: &mut State) -> io::Result<()> {
        let end = state
            .pending_end
            .take()
            .ok_or_else(|| io::Error::other("missing Turso commit boundary"))?;
        let start = self.inner.logical_log_offset();
        if !state.refresh_base && state.layout.log != start {
            return Err(io::Error::other(
                "Turso logical log moved outside a checkpoint",
            ));
        }
        let layout = Layout {
            base: fs::metadata(self.dir.join("data.db"))?.len(),
            log: end,
        };
        if !state.refresh_base && layout.base != state.layout.base {
            return Err(io::Error::other("Turso base changed outside a checkpoint"));
        }
        let pages = layout.changes(&self.dir, start, state.refresh_base)?;
        state
            .writer
            .publish_pages(PAGE as u32, layout.size()?, pages, state.layout.base == 0)?;
        state.published = true;
        state.layout = layout;
        state.refresh_base = false;
        Ok(())
    }
}

impl DurableStorage for ReplicatedLog {
    fn log_tx(&self, m: LogRecord, cb: OnSerializationComplete<'_>) -> Result<(Completion, u64)> {
        let (completion, len) = self.inner.log_tx(m, cb)?;
        let end = self
            .inner
            .logical_log_offset()
            .checked_add(len)
            .ok_or_else(|| LimboError::InternalError("Turso log offset overflow".into()))?;
        self.state.lock().pending_end = Some(end);
        Ok((completion, len))
    }
    fn on_log_write_complete(&self) -> Result<Completion> {
        let mut state = self.state.lock();
        if let Err(error) = self.publish(&mut state) {
            state.writer.fence();
            return Err(LimboError::InternalError(format!(
                "Raft commit failed: {error}"
            )));
        }
        Ok(Completion::new_yield())
    }
    fn on_checkpoint_start(&self) -> Result<()> {
        self.state.lock().refresh_base = true;
        Ok(())
    }
    fn on_checkpoint_end(&self, result: Result<&CheckpointResult>) -> Result<()> {
        if result.is_err() {
            self.state.lock().writer.fence();
        }
        result.map(|_| ())
    }
    fn serialize_row_version(
        &self,
        r: &mut LogRecord,
        id: &RowID,
        v: &RowVersion,
        ext: Option<&[u8]>,
    ) -> Result<()> {
        self.inner.serialize_row_version(r, id, v, ext)
    }
    fn serialize_database_header(&self, r: &mut LogRecord, h: &DatabaseHeader) -> Result<()> {
        self.inner.serialize_database_header(r, h)
    }
    fn upgrade_header_for_log_tx(&self, r: &LogRecord) -> Result<Option<Completion>> {
        self.inner.upgrade_header_for_log_tx(r)
    }
    fn sync(&self, s: FileSyncType) -> Result<Completion> {
        self.inner.sync(s)
    }
    fn update_header(&self) -> Result<Completion> {
        self.inner.update_header()
    }
    fn truncate(&self, ts: u64) -> Result<(Completion, LogicalLogTruncateOutcome)> {
        self.inner.truncate(ts)
    }
    fn reset_to_fresh_header(&self) -> Result<Completion> {
        self.inner.reset_to_fresh_header()
    }
    fn get_logical_log_file(&self) -> Arc<dyn File> {
        self.inner.get_logical_log_file()
    }
    fn logical_log_offset(&self) -> u64 {
        self.inner.logical_log_offset()
    }
    fn should_checkpoint(&self) -> bool {
        self.inner.should_checkpoint()
    }
    fn set_checkpoint_threshold(&self, t: i64) {
        self.inner.set_checkpoint_threshold(t)
    }
    fn checkpoint_threshold(&self) -> i64 {
        self.inner.checkpoint_threshold()
    }
    fn advance_logical_log_offset_after_success(&self, n: u64) -> Result<()> {
        let result = self.inner.advance_logical_log_offset_after_success(n);
        let mut state = self.state.lock();
        if result.is_err() {
            state.writer.fence();
        }
        state.published = false;
        result
    }
    fn discard_pending_log_write(&self) -> Result<()> {
        let mut state = self.state.lock();
        if state.published {
            state.writer.fence();
        }
        state.pending_end = None;
        self.inner.discard_pending_log_write()
    }
    fn restore_logical_log_state_after_recovery(&self, n: u64, crc: u32) {
        self.inner.restore_logical_log_state_after_recovery(n, crc)
    }
    fn set_header(&self, h: LogHeader) {
        self.inner.set_header(h)
    }
}
