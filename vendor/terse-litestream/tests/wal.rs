use super::*;
use std::io::Cursor;

#[test]
fn only_committed_frames_enter_the_page_map() {
    for big in [false, true] {
        let bytes = fixture(big, &[(1, 0, 11), (2, 2, 22), (1, 0, 33)]);
        let mut wal = WalReader::new(Cursor::new(bytes)).unwrap();
        let map = wal.scan(0).unwrap();
        assert_eq!(map.commit, 2);
        assert_eq!(map.end, 32 + 2 * 536);
        assert_eq!(map.pages.into_keys().collect::<Vec<_>>(), [1, 2]);
    }
}

#[test]
fn corrupt_and_partial_tails_stop_at_the_last_commit() {
    let original = fixture(false, &[(1, 1, 11), (1, 1, 22)]);
    for cut in [32 + 536, 32 + 536 + 1, original.len() - 1] {
        let map = WalReader::new(Cursor::new(original[..cut].to_vec()))
            .unwrap()
            .scan(0)
            .unwrap();
        assert_eq!(map.end, 32 + 536);
    }
    for offset in [32 + 536 + 8, 32 + 536 + 16, original.len() - 1] {
        let mut bytes = original.clone();
        bytes[offset] ^= 1;
        let map = WalReader::new(Cursor::new(bytes)).unwrap().scan(0).unwrap();
        assert_eq!(map.end, 32 + 536);
    }
}

#[test]
fn byte_limit_stops_at_a_commit_and_shrink_discards_pages() {
    let bytes = fixture(false, &[(1, 0, 1), (2, 2, 2), (1, 1, 3)]);
    let mut reader = WalReader::new(Cursor::new(bytes)).unwrap();
    let first = reader.scan(1).unwrap();
    assert_eq!(first.commit, 2);
    assert_eq!(first.pages.len(), 2);
    let second = reader.scan(0).unwrap();
    assert_eq!(second.commit, 1);
    assert_eq!(second.pages.into_keys().collect::<Vec<_>>(), [1]);
}

#[test]
fn invalid_headers_fail_without_allocating_from_untrusted_sizes() {
    for (offset, value) in [(0, 0), (4, 42), (8, 0), (8, u32::MAX), (8, 513)] {
        let mut bytes = fixture(false, &[]);
        bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
        let sum = checksum(false, [0, 0], &bytes[..24]);
        bytes[24..28].copy_from_slice(&sum[0].to_be_bytes());
        bytes[28..32].copy_from_slice(&sum[1].to_be_bytes());
        assert!(WalReader::new(Cursor::new(bytes)).is_err());
    }
}

fn fixture(big: bool, frames: &[(u32, u32, u8)]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for word in [0x377f0682 + u32::from(big), 3007000, 512, 0, 17, 23] {
        bytes.extend_from_slice(&word.to_be_bytes());
    }
    let mut sum = checksum(big, [0, 0], &bytes);
    for word in sum {
        bytes.extend_from_slice(&word.to_be_bytes());
    }
    for &(number, commit, value) in frames {
        let mut frame = Vec::new();
        for word in [number, commit, 17, 23] {
            frame.extend_from_slice(&word.to_be_bytes());
        }
        let data = vec![value; 512];
        sum = checksum(big, sum, &frame[..8]);
        sum = checksum(big, sum, &data);
        for word in sum {
            frame.extend_from_slice(&word.to_be_bytes());
        }
        frame.extend(data);
        bytes.extend(frame);
    }
    bytes
}

#[test]
fn upstream_wal_fixtures_and_every_truncated_prefix_match_committed_boundaries() {
    let valid = include_bytes!("fixtures/wal/valid.wal");
    for cut in 0..=valid.len() {
        let reader = WalReader::new(Cursor::new(&valid[..cut]));
        if cut < 32 {
            assert!(reader.is_err());
            continue;
        }
        let map = reader.unwrap().scan(0).unwrap();
        let expected = if cut < 8272 {
            0
        } else if cut < 12392 {
            8272
        } else {
            12392
        };
        assert_eq!(map.end, expected, "prefix {cut}");
        if expected > 0 {
            assert_eq!(map.commit, 2);
            assert_eq!(map.pages[&1], 32);
            assert_eq!(map.pages[&2], expected - 4120);
        } else {
            assert!(map.pages.is_empty());
        }
    }
    for invalid in [
        include_bytes!("fixtures/wal/salt-mismatch.wal"),
        include_bytes!("fixtures/wal/checksum-mismatch.wal"),
    ] {
        let map = WalReader::new(Cursor::new(invalid))
            .unwrap()
            .scan(0)
            .unwrap();
        assert_eq!(map.end, 0);
        assert!(map.pages.is_empty());
    }
}

#[test]
fn resume_requires_the_previous_committed_frame_and_checksum_chain() {
    let bytes = include_bytes!("fixtures/wal/valid.wal");
    let mut reader = WalReader::new(Cursor::new(bytes)).unwrap();
    let first = reader.scan(1).unwrap();
    let mut resumed = WalReader::new(Cursor::new(bytes)).unwrap();
    assert!(resumed.resume(first.end, &first.last_frame).unwrap());
    let next = resumed.scan(0).unwrap();
    assert_eq!(next.pages.into_iter().collect::<Vec<_>>(), [(2, 8272)]);
    assert_eq!(next.end, 12392);
    let mut changed = bytes.to_vec();
    changed[8000] ^= 1;
    assert!(
        !WalReader::new(Cursor::new(changed))
            .unwrap()
            .resume(first.end, &first.last_frame)
            .unwrap()
    );
    assert!(
        !WalReader::new(Cursor::new(&bytes[..8000]))
            .unwrap()
            .resume(first.end, &first.last_frame)
            .unwrap()
    );
    assert!(resumed.resume(first.end + 1, &first.last_frame).is_err());
}
