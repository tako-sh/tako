use std::{
    fs,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use futures::{TryStreamExt, stream};
use object_store::{ObjectStore, path::Path as ObjectPath};
use terse_litestream::{
    Durability, FileStore, ReplicaStore, Segment, compact, restore_plan, restore_with,
};
use terse_ltx::Decoder;

use crate::{receipt_path, segment_key, upload_lock, upload_segment};

/// Keep all available capture points from the last seven days, plus their base.
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Cleanup is daily; steady-state retained history can therefore approach eight days.
pub const RETENTION_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Rolls expired history into one verified base before deleting remote or local inputs.
/// Uses local LTX timestamps and receipts; maintenance never LISTs or HEADs the remote store.
pub async fn prune_expired(
    spool: &FileStore,
    remote: &dyn ObjectStore,
    prefix: &ObjectPath,
    now: SystemTime,
) -> Result<usize> {
    let _lock = upload_lock(spool, remote, prefix)?;
    let Some(cutoff) = now.checked_sub(RETENTION) else {
        return Ok(0);
    };
    let cutoff = i64::try_from(cutoff.duration_since(UNIX_EPOCH)?.as_millis())?;
    let mut segments = spool.list(0)?;
    segments.extend(spool.list(9)?);
    segments.sort_by_key(|s| (s.max_txid, s.level));
    let mut target = None;
    for segment in &segments {
        let decoder = Decoder::new(spool.open(segment)?)?;
        let header = decoder.header();
        ensure!(
            header.min_txid == segment.min_txid && header.max_txid == segment.max_txid,
            "LTX filename and header disagree"
        );
        // Stop at the first retained point, including when the capture clock moved backwards.
        if header.timestamp >= cutoff {
            break;
        }
        target = Some(segment.max_txid);
    }
    let Some(target) = target else { return Ok(0) };
    let anchor = Segment::new(9, 1, target)?;
    let obsolete: Vec<_> = segments
        .into_iter()
        .filter(|s| s.max_txid <= target && *s != anchor)
        .collect();
    if obsolete.is_empty()
        || (!spool.path(&anchor).exists() && obsolete.len() == 1 && obsolete[0].min_txid == 1)
    {
        return Ok(0);
    }
    if !spool.path(&anchor).exists() {
        compact(spool, &restore_plan(spool, Some(target))?, 9)?;
    }
    // Verify the replacement on its own, before relying on its receipt or deleting inputs.
    let check = tempfile::tempdir()?;
    restore_with(
        spool,
        check.path().join("anchor.sqlite"),
        Some(target),
        Durability::Scratch,
    )
    .context("verify retention anchor")?;
    upload_segment(spool, remote, prefix, &anchor).await?;

    let paths = obsolete
        .iter()
        .map(|s| Ok(segment_key(prefix, s)))
        .collect::<Vec<_>>();
    let mut deletions = remote.delete_stream(Box::pin(stream::iter(paths)));
    let mut removed = 0;
    for segment in obsolete {
        match deletions.try_next().await {
            Ok(Some(_)) | Err(object_store::Error::NotFound { .. }) => {}
            Ok(None) => anyhow::bail!("object store did not acknowledge all deletions"),
            Err(error) => return Err(error.into()),
        }
        // Keep the receipt until local deletion is durable, so interruption cannot re-upload it.
        spool.remove(&segment)?;
        match fs::remove_file(receipt_path(spool, &segment)) {
            Ok(()) => fs::File::open(spool.root().join("uploaded"))?.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        removed += 1;
    }
    Ok(removed)
}
