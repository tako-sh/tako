use anyhow::{Result, ensure};
use std::{
    collections::BTreeMap,
    io::{self, Read, Seek, SeekFrom},
};

pub(crate) const HEADER_SIZE: u64 = 32;
pub(crate) const FRAME_HEADER_SIZE: u64 = 24;

pub(crate) struct WalReader<R> {
    input: R,
    pub page_size: u32,
    pub salt: [u32; 2],
    big: bool,
    sum: [u32; 2],
    offset: u64,
}

#[derive(Default)]
pub(crate) struct PageMap {
    pub pages: BTreeMap<u32, u64>,
    pub commit: u32,
    pub end: u64,
    pub last_frame: Vec<u8>,
    pub limited: bool,
}

impl<R: Read + Seek> WalReader<R> {
    pub fn new(mut input: R) -> Result<Self> {
        let mut header = [0; 32];
        input.read_exact(&mut header)?;
        let magic = word(&header, 0);
        ensure!(
            matches!(magic, 0x377f0682 | 0x377f0683),
            "invalid WAL magic"
        );
        let big = magic == 0x377f0683;
        let sum = [word(&header, 24), word(&header, 28)];
        ensure!(
            checksum(big, [0, 0], &header[..24]) == sum,
            "invalid WAL header checksum"
        );
        ensure!(word(&header, 4) == 3007000, "unsupported WAL version");
        let page_size = word(&header, 8);
        ensure!(
            (512..=65536).contains(&page_size) && page_size.is_power_of_two(),
            "invalid WAL page size"
        );
        Ok(Self {
            input,
            page_size,
            salt: [word(&header, 16), word(&header, 20)],
            big,
            sum,
            offset: HEADER_SIZE,
        })
    }

    pub fn resume(&mut self, end: u64, previous: &[u8]) -> Result<bool> {
        let size = self.frame_size();
        ensure!(
            end >= HEADER_SIZE + size && (end - HEADER_SIZE) % size == 0,
            "unaligned WAL offset"
        );
        if previous.len() != size as usize {
            return Ok(false);
        }
        let mut frame = vec![0; size as usize];
        self.input.seek(SeekFrom::Start(end - size))?;
        if !read_complete(&mut self.input, &mut frame)? || frame != previous {
            return Ok(false);
        }
        if [word(&frame, 8), word(&frame, 12)] != self.salt {
            return Ok(false);
        }
        self.sum = [word(&frame, 16), word(&frame, 20)];
        self.offset = end;
        Ok(true)
    }

    pub fn scan(&mut self, max_bytes: u64) -> Result<PageMap> {
        let mut result = PageMap::default();
        let mut pending = BTreeMap::new();
        let mut frame = vec![0; self.frame_size() as usize];
        let start = self.offset;
        self.input.seek(SeekFrom::Start(start))?;
        while read_complete(&mut self.input, &mut frame)? {
            if [word(&frame, 8), word(&frame, 12)] != self.salt {
                break;
            }
            let sum = checksum(
                self.big,
                checksum(self.big, self.sum, &frame[..8]),
                &frame[24..],
            );
            if sum != [word(&frame, 16), word(&frame, 20)] {
                break;
            }
            let number = word(&frame, 0);
            ensure!(number > 0, "invalid WAL page number");
            pending.insert(number, self.offset);
            self.offset += self.frame_size();
            self.sum = sum;
            let commit = word(&frame, 4);
            if commit == 0 {
                continue;
            }
            result.pages.append(&mut pending);
            result.commit = commit;
            result.end = self.offset;
            result.last_frame.clone_from(&frame);
            if max_bytes > 0 && self.offset - start >= max_bytes {
                result.limited = true;
                break;
            }
        }
        result.pages.retain(|number, _| *number <= result.commit);
        Ok(result)
    }

    fn frame_size(&self) -> u64 {
        u64::from(self.page_size) + FRAME_HEADER_SIZE
    }
}

fn read_complete(input: &mut impl Read, bytes: &mut [u8]) -> io::Result<bool> {
    match input.read_exact(bytes) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error),
    }
}

fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn checksum(big: bool, mut sum: [u32; 2], bytes: &[u8]) -> [u32; 2] {
    for pair in bytes.chunks_exact(8) {
        let read = |slice: &[u8]| {
            let bytes = slice.try_into().unwrap();
            if big {
                u32::from_be_bytes(bytes)
            } else {
                u32::from_le_bytes(bytes)
            }
        };
        sum[0] = sum[0].wrapping_add(read(&pair[..4])).wrapping_add(sum[1]);
        sum[1] = sum[1].wrapping_add(read(&pair[4..])).wrapping_add(sum[0]);
    }
    sum
}

#[cfg(test)]
#[path = "../tests/wal.rs"]
mod tests;
