use super::IMAGE_WORKER_FRAME_MAX_BYTES;
use std::io::{ErrorKind, Read, Write};
use tako_images::ImageError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, ChildStdout};

pub(super) async fn write_worker_frame_async(
    writer: &mut ChildStdin,
    bytes: &[u8],
) -> Result<(), ImageError> {
    let len = worker_frame_len(bytes.len()).map_err(|_| ImageError::TransformFailed)?;
    writer
        .write_all(&len.to_be_bytes())
        .await
        .map_err(|_| ImageError::TransformFailed)?;
    writer
        .write_all(bytes)
        .await
        .map_err(|_| ImageError::TransformFailed)?;
    writer
        .flush()
        .await
        .map_err(|_| ImageError::TransformFailed)
}

pub(super) async fn read_worker_frame_async(
    reader: &mut ChildStdout,
) -> Result<Vec<u8>, ImageError> {
    let mut len = [0_u8; 4];
    reader
        .read_exact(&mut len)
        .await
        .map_err(|_| ImageError::TransformFailed)?;
    let len = usize::try_from(u32::from_be_bytes(len)).map_err(|_| ImageError::TransformFailed)?;
    if len > IMAGE_WORKER_FRAME_MAX_BYTES {
        return Err(ImageError::TransformFailed);
    }
    let mut output = vec![0_u8; len];
    reader
        .read_exact(&mut output)
        .await
        .map_err(|_| ImageError::TransformFailed)?;
    Ok(output)
}

pub(super) fn read_worker_frame_sync(reader: &mut impl Read) -> Result<Option<Vec<u8>>, String> {
    let mut len = [0_u8; 4];
    match read_exact_or_clean_eof(reader, &mut len)? {
        Some(()) => {}
        None => return Ok(None),
    }
    let len = usize::try_from(u32::from_be_bytes(len))
        .map_err(|_| "image worker frame length is invalid".to_string())?;
    if len > IMAGE_WORKER_FRAME_MAX_BYTES {
        return Err("image worker frame is too large".to_string());
    }
    let mut input = vec![0_u8; len];
    reader
        .read_exact(&mut input)
        .map_err(|error| format!("read image worker frame body: {error}"))?;
    Ok(Some(input))
}

pub(super) fn write_worker_frame_sync(writer: &mut impl Write, bytes: &[u8]) -> Result<(), String> {
    let len =
        worker_frame_len(bytes.len()).map_err(|_| "image worker frame is too large".to_string())?;
    writer
        .write_all(&len.to_be_bytes())
        .map_err(|error| format!("write image worker frame length: {error}"))?;
    writer
        .write_all(bytes)
        .map_err(|error| format!("write image worker frame body: {error}"))?;
    writer
        .flush()
        .map_err(|error| format!("flush image worker frame: {error}"))
}

fn read_exact_or_clean_eof(
    reader: &mut impl Read,
    buffer: &mut [u8],
) -> Result<Option<()>, String> {
    let mut read = 0;
    while read < buffer.len() {
        match reader.read(&mut buffer[read..]) {
            Ok(0) if read == 0 => return Ok(None),
            Ok(0) => return Err("image worker frame ended early".to_string()),
            Ok(n) => read += n,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("read image worker frame length: {error}")),
        }
    }
    Ok(Some(()))
}

fn worker_frame_len(len: usize) -> Result<u32, ()> {
    if len > IMAGE_WORKER_FRAME_MAX_BYTES {
        return Err(());
    }
    u32::try_from(len).map_err(|_| ())
}
