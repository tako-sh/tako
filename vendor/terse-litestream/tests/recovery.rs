use anyhow::Result;
use std::{fs, io::Write};
use terse_litestream::{FileStore, ReplicaStore, Segment, restore};

#[test]
fn malformed_unicode_filenames_are_ignored() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = FileStore::new(dir.path());
    fs::create_dir_all(dir.path().join("ltx/0"))?;
    let name = format!("{}é{}.ltx", "x".repeat(15), "x".repeat(16));
    fs::write(dir.path().join("ltx/0").join(name), b"invalid")?;
    assert!(store.list(0)?.is_empty());
    Ok(())
}

#[test]
fn checksummed_restore_rejects_a_false_database_checksum() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = FileStore::new(dir.path().join("replica"));
    let page = vec![7u8; 512];
    let crc = crc::Crc::<u64>::new(&crc::CRC_64_GO_ISO);
    let mut hash = crc.digest();
    hash.update(&1u32.to_be_bytes());
    hash.update(&page);
    let sum = hash.finalize() | terse_ltx::CHECKSUM_FLAG;
    let first = Segment::new(0, 1, 1)?;
    let mut encoder = terse_ltx::Encoder::new(
        store.create(&first)?,
        terse_ltx::Header {
            page_size: 512,
            commit: 1,
            min_txid: 1,
            max_txid: 1,
            ..Default::default()
        },
    )?;
    encoder.write_page(1, &page)?;
    encoder.finish(sum)?.commit()?;
    let valid = dir.path().join("valid.db");
    restore(&store, &valid, Some(1))?;
    assert_eq!(fs::read(valid)?, page);
    let second = Segment::new(0, 2, 2)?;
    let mut encoder = terse_ltx::Encoder::new(
        store.create(&second)?,
        terse_ltx::Header {
            page_size: 512,
            commit: 1,
            min_txid: 2,
            max_txid: 2,
            pre_apply_checksum: sum,
            ..Default::default()
        },
    )?;
    encoder.write_page(1, &vec![8u8; 512])?;
    encoder.finish(sum)?.commit()?;
    let output = dir.path().join("output.db");
    assert!(restore(&store, &output, None).is_err());
    assert!(!output.exists());
    Ok(())
}

#[test]
fn published_segments_cannot_be_replaced() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = FileStore::new(dir.path());
    let segment = Segment::new(0, 1, 1)?;
    let mut first = store.create(&segment)?;
    first.write_all(b"first")?;
    first.commit()?;
    let mut second = store.create(&segment)?;
    second.write_all(b"second")?;
    assert!(second.commit().is_err());
    assert_eq!(fs::read(store.path(&segment))?, b"first");
    Ok(())
}

#[test]
fn replicated_and_compacted_pages_restore_at_the_requested_transaction() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = FileStore::new(dir.path().join("source"));
    let replica = FileStore::new(dir.path().join("replica"));
    for txid in 1..=3 {
        let segment = Segment::new(0, txid, txid)?;
        let mut encoder = terse_ltx::Encoder::new(
            source.create(&segment)?,
            terse_ltx::Header {
                flags: terse_ltx::NO_CHECKSUM,
                page_size: 512,
                commit: 1,
                min_txid: txid,
                max_txid: txid,
                ..Default::default()
            },
        )?;
        encoder.write_page(1, &[txid as u8; 512])?;
        encoder.finish(0)?.commit()?;
    }
    assert_eq!(terse_litestream::replicate(&source, &replica)?, 3);
    let segments = replica.list(0)?;
    let segment_path = replica.path(&segments[1]);
    let original = fs::read(&segment_path)?;
    fs::write(&segment_path, &original[..original.len() / 2])?;
    let latest = dir.path().join("latest.db");
    assert!(restore(&replica, &latest, None).is_err());
    assert!(!latest.exists());
    fs::write(segment_path, original)?;

    let earlier = dir.path().join("earlier.db");
    assert_eq!(restore(&replica, &earlier, Some(1))?, 1);
    assert!(restore(&replica, &earlier, None).is_err());
    assert_eq!(fs::read(earlier)?, [1; 512]);

    terse_litestream::compact(&replica, &segments[..2], 1)?;
    for segment in &segments[..2] {
        replica.remove(segment)?;
    }
    let at_two = dir.path().join("at-two.db");
    assert_eq!(restore(&replica, &at_two, Some(2))?, 2);
    assert_eq!(fs::read(at_two)?, [2; 512]);
    assert_eq!(restore(&replica, &latest, None)?, 3);
    assert_eq!(fs::read(latest)?, [3; 512]);
    Ok(())
}
