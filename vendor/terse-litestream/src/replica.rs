use crate::{
    Durability, ReplicaStore, Segment,
    store::{create_directory, sync_directory},
};
use anyhow::{Context, Result, ensure};
use std::{
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};
use tempfile::NamedTempFile;
use terse_ltx::{Decoder, Header};

pub fn replicate(source: &dyn ReplicaStore, destination: &dyn ReplicaStore) -> Result<u64> {
    let remote = latest_segment(destination)?;
    let mut txid = remote.map_or(0, |segment| segment.max_txid);
    let local = all_segments(source)?;
    let end = local
        .iter()
        .map(|segment| segment.max_txid)
        .max()
        .unwrap_or(0);
    ensure!(
        txid <= end,
        "local capture is behind the replica; recover its position first"
    );
    while txid < end {
        let next = txid.checked_add(1).context("transaction overflow")?;
        let segment = local
            .iter()
            .filter(|segment| segment.min_txid <= next && segment.max_txid > txid)
            .max_by_key(|segment| segment.max_txid)
            .context("replication transaction gap")?;
        validate_segment(source, segment)?;
        let mut output = destination.create(segment)?;
        io::copy(&mut source.open(segment)?, &mut output)?;
        output.commit()?;
        txid = segment.max_txid;
    }
    Ok(txid)
}

pub fn restore(
    store: &dyn ReplicaStore,
    output: impl AsRef<Path>,
    target: Option<u64>,
) -> Result<u64> {
    restore_with(store, output, target, Durability::Synced)
}

/// Restores like [`restore`]; a scratch restore publishes the database without fsync.
pub fn restore_with(
    store: &dyn ReplicaStore,
    output: impl AsRef<Path>,
    target: Option<u64>,
    durability: Durability,
) -> Result<u64> {
    let plan = restore_plan(store, target)?;
    let output = output.as_ref();
    ensure!(!output.exists(), "restore output already exists");
    let parent = output.parent().context("restore parent missing")?;
    create_directory(parent)?;
    let mut file = NamedTempFile::new_in(parent)?;
    let mut previous: Option<(Header, u64)> = None;
    for segment in &plan {
        apply_segment(store, segment, &mut file, &mut previous)?;
    }
    if let Some((header, checksum)) = previous {
        verify_database_checksum(&mut file, &header, checksum)?;
    }
    file.flush()?;
    let synced = durability == Durability::Synced;
    if synced {
        file.as_file().sync_all()?;
    }
    file.persist_noclobber(output)?;
    if synced {
        sync_directory(parent)?;
    }
    Ok(plan.last().context("empty restore plan")?.max_txid)
}

pub fn restore_plan(store: &dyn ReplicaStore, target: Option<u64>) -> Result<Vec<Segment>> {
    ensure!(target != Some(0), "restore target must be positive");
    let files = all_segments(store)?;
    let target = target
        .or_else(|| files.iter().map(|s| s.max_txid).max())
        .context("no replica files")?;
    let mut end = 0u64;
    let mut plan = Vec::new();
    while end < target {
        let next = end.checked_add(1).context("transaction overflow")?;
        let segment = files
            .iter()
            .filter(|file| file.min_txid <= next && file.max_txid > end && file.max_txid <= target)
            .max_by_key(|file| (file.max_txid, file.level))
            .context("replica has a transaction gap or target is unavailable")?;
        if end == 0 {
            ensure!(segment.min_txid == 1, "replica has no base snapshot");
        }
        end = segment.max_txid;
        plan.push(*segment);
    }
    Ok(plan)
}

pub fn compact(store: &dyn ReplicaStore, segments: &[Segment], level: u8) -> Result<Segment> {
    let first = segments.first().context("no compaction input")?;
    let last = segments.last().context("no compaction input")?;
    let output = Segment::new(level, first.min_txid, last.max_txid)?;
    ensure!(
        !segments.contains(&output),
        "compaction would overwrite an input"
    );
    let mut inputs = Vec::new();
    for segment in segments {
        validate_segment(store, segment)?;
        inputs.push(store.open(segment)?);
    }
    let mut writer = store.create(&output)?;
    let header = terse_ltx::compact(inputs, &mut writer)?;
    ensure!(
        header.min_txid == output.min_txid && header.max_txid == output.max_txid,
        "compaction range mismatch"
    );
    writer.commit()?;
    Ok(output)
}

pub(crate) fn validate_segment(store: &dyn ReplicaStore, segment: &Segment) -> Result<Header> {
    let mut decoder = checked_decoder(store, segment)?;
    while decoder.next_page()?.is_some() {}
    Ok(*decoder.header())
}

pub(crate) fn latest_segment(store: &dyn ReplicaStore) -> Result<Option<Segment>> {
    Ok(all_segments(store)?
        .into_iter()
        .max_by_key(|segment| (segment.max_txid, segment.level)))
}

fn all_segments(store: &dyn ReplicaStore) -> Result<Vec<Segment>> {
    let mut files = Vec::new();
    for level in 0..=9 {
        files.extend(store.list(level)?);
    }
    for file in &files {
        file.validate()?;
    }
    Ok(files)
}

fn checked_decoder(
    store: &dyn ReplicaStore,
    segment: &Segment,
) -> Result<Decoder<Box<dyn Read + Send>>> {
    let decoder = Decoder::new(store.open(segment)?)?;
    ensure!(
        decoder.header().min_txid == segment.min_txid
            && decoder.header().max_txid == segment.max_txid,
        "LTX filename and header disagree"
    );
    Ok(decoder)
}

fn apply_segment(
    store: &dyn ReplicaStore,
    segment: &Segment,
    output: &mut NamedTempFile,
    previous: &mut Option<(Header, u64)>,
) -> Result<()> {
    let mut decoder = checked_decoder(store, segment)?;
    let header = *decoder.header();
    if let Some((before, checksum)) = previous {
        ensure!(
            before.page_size == header.page_size && before.flags == header.flags,
            "LTX format changed within restore chain"
        );
        if header.flags & terse_ltx::NO_CHECKSUM == 0
            && before.max_txid.checked_add(1) == Some(header.min_txid)
        {
            ensure!(
                *checksum == header.pre_apply_checksum,
                "LTX transaction checksum mismatch"
            );
        }
    }
    while let Some(page) = decoder.next_page()? {
        output.seek(SeekFrom::Start(
            u64::from(page.number - 1) * u64::from(header.page_size),
        ))?;
        output.write_all(&page.data)?;
    }
    output
        .as_file()
        .set_len(u64::from(header.commit) * u64::from(header.page_size))?;
    *previous = Some((
        header,
        decoder
            .trailer()
            .context("missing LTX trailer")?
            .post_apply_checksum,
    ));
    Ok(())
}

fn verify_database_checksum(
    file: &mut NamedTempFile,
    header: &Header,
    expected: u64,
) -> Result<()> {
    if header.flags & terse_ltx::NO_CHECKSUM != 0 {
        return Ok(());
    }
    let crc = crc::Crc::<u64>::new(&crc::CRC_64_GO_ISO);
    let mut checksum = terse_ltx::CHECKSUM_FLAG;
    let mut page = vec![0; header.page_size as usize];
    file.seek(SeekFrom::Start(0))?;
    for number in 1..=header.commit {
        file.read_exact(&mut page)?;
        if number == 0x40000000 / header.page_size + 1 {
            continue;
        }
        let mut hash = crc.digest();
        hash.update(&number.to_be_bytes());
        hash.update(&page);
        checksum = (checksum ^ hash.finalize()) | terse_ltx::CHECKSUM_FLAG;
    }
    ensure!(checksum == expected, "restored database checksum mismatch");
    Ok(())
}
