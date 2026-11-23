//! Authenticated encryption for immutable replica objects. Object names are
//! authenticated too, so a valid segment cannot be substituted at another key.
use std::{
    fmt,
    io::{Read, Seek, SeekFrom, Write},
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, stream, stream::BoxStream};
use object_store::{
    CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectMeta,
    ObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult, path::Path as ObjectPath,
};
use openssl::symm::{Cipher, Crypter, Mode, decrypt_aead, encrypt_aead};
use zeroize::Zeroize;

const MAGIC: &[u8; 8] = b"TAKOLTX1";

/// The most an object holds: it is sealed as one AES-GCM message, which
/// encrypts at most 2^39 - 256 bits (RFC 5116), just under 64 GiB.
pub const MAX_SEALED_BYTES: u64 = (1 << 36) - 32;

/// Fails for a payload larger than one object can hold.
pub fn ensure_sealable(bytes: u64) -> Result<()> {
    ensure!(
        bytes <= MAX_SEALED_BYTES,
        "{bytes} bytes is more than one encrypted object holds ({MAX_SEALED_BYTES} bytes, the AES-GCM limit)"
    );
    Ok(())
}

#[derive(Clone)]
pub struct Key {
    pub id: String,
    bytes: [u8; 32],
}

impl Key {
    pub fn new(id: String, bytes: [u8; 32]) -> Result<Self> {
        ensure!(!id.is_empty() && id.len() <= 256, "invalid replica key id");
        Ok(Self { id, bytes })
    }

    pub fn generate(id: String) -> Result<Self> {
        let mut bytes = [0; 32];
        openssl::rand::rand_bytes(&mut bytes)?;
        Self::new(id, bytes)
    }

    pub fn bytes(&self) -> &[u8; 32] {
        &self.bytes
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Key")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

pub fn seal(key: &Key, context: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut nonce = [0; 12];
    openssl::rand::rand_bytes(&mut nonce)?;
    let mut tag = [0; 16];
    let ciphertext = encrypt_aead(
        Cipher::aes_256_gcm(),
        &key.bytes,
        Some(&nonce),
        context,
        plaintext,
        &mut tag,
    )?;
    let mut out = Vec::with_capacity(38 + key.id.len() + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(key.id.len() as u16).to_be_bytes());
    out.extend_from_slice(key.id.as_bytes());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&tag);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

pub fn open(keys: &[Key], context: &[u8], bytes: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        bytes.len() >= 38 && &bytes[..8] == MAGIC,
        "invalid encrypted replica object"
    );
    let length = u16::from_be_bytes([bytes[8], bytes[9]]) as usize;
    ensure!(
        (1..=256).contains(&length) && bytes.len() >= 38 + length,
        "invalid replica key header"
    );
    let id = std::str::from_utf8(&bytes[10..10 + length])?;
    let key = keys
        .iter()
        .find(|key| key.id == id)
        .context("replica decryption key is unavailable")?;
    let at = 10 + length;
    decrypt_aead(
        Cipher::aes_256_gcm(),
        &key.bytes,
        Some(&bytes[at..at + 12]),
        context,
        &bytes[at + 28..],
        &bytes[at + 12..at + 28],
    )
    .context("replica object authentication failed")
}

// Crypto scratch files are unlinked and mode 0600. No plaintext is returned
// until GCM authentication succeeds. The immutable mappings keep resident
// memory bounded while preserving create-only, idempotent single-object PUTs.
fn mapped(file: &std::fs::File) -> Result<Bytes> {
    if file.metadata()?.len() == 0 {
        return Ok(Bytes::new());
    }
    // This file is private, unlinked, and never modified after mapping.
    Ok(unsafe { Bytes::from_owner(memmap2::Mmap::map(file)?) })
}

fn seal_payload(key: &Key, context: &[u8], payload: PutPayload) -> Result<Bytes> {
    ensure_sealable(payload.content_length() as u64)?;
    let mut file = tempfile::tempfile()?;
    let mut nonce = [0; 12];
    openssl::rand::rand_bytes(&mut nonce)?;
    file.write_all(MAGIC)?;
    file.write_all(&(key.id.len() as u16).to_be_bytes())?;
    file.write_all(key.id.as_bytes())?;
    file.write_all(&nonce)?;
    file.write_all(&[0; 16])?;
    let mut cipher = Crypter::new(
        Cipher::aes_256_gcm(),
        Mode::Encrypt,
        key.bytes(),
        Some(&nonce),
    )?;
    cipher.aad_update(context)?;
    let mut output = vec![0; 256 * 1024 + 16];
    for bytes in payload.iter() {
        for chunk in bytes.chunks(256 * 1024) {
            let size = cipher.update(chunk, &mut output)?;
            file.write_all(&output[..size])?;
        }
    }
    let size = cipher.finalize(&mut output)?;
    file.write_all(&output[..size])?;
    let mut tag = [0; 16];
    cipher.get_tag(&mut tag)?;
    file.seek(SeekFrom::Start((22 + key.id.len()) as u64))?;
    file.write_all(&tag)?;
    mapped(&file)
}

fn open_file(keys: &[Key], context: &[u8], mut input: std::fs::File) -> Result<Bytes> {
    input.rewind()?;
    let mut header = [0; 10];
    input.read_exact(&mut header)?;
    ensure!(&header[..8] == MAGIC, "invalid encrypted replica object");
    let length = u16::from_be_bytes([header[8], header[9]]) as usize;
    ensure!((1..=256).contains(&length), "invalid replica key header");
    let mut id = vec![0; length];
    input.read_exact(&mut id)?;
    let id = std::str::from_utf8(&id)?;
    let key = keys
        .iter()
        .find(|key| key.id == id)
        .context("replica decryption key is unavailable")?;
    let mut nonce = [0; 12];
    let mut tag = [0; 16];
    input.read_exact(&mut nonce)?;
    input.read_exact(&mut tag)?;
    let mut cipher = Crypter::new(
        Cipher::aes_256_gcm(),
        Mode::Decrypt,
        key.bytes(),
        Some(&nonce),
    )?;
    cipher.aad_update(context)?;
    cipher.set_tag(&tag)?;
    let mut output = tempfile::tempfile()?;
    let mut chunk = vec![0; 256 * 1024];
    let mut plaintext = vec![0; chunk.len() + 16];
    loop {
        let size = input.read(&mut chunk)?;
        if size == 0 {
            break;
        }
        let size = cipher.update(&chunk[..size], &mut plaintext)?;
        output.write_all(&plaintext[..size])?;
    }
    let size = cipher
        .finalize(&mut plaintext)
        .context("replica object authentication failed")?;
    output.write_all(&plaintext[..size])?;
    mapped(&output)
}

/// Full-object adapter used by capture uploads and restores. Randomized retries
/// remain idempotent: a lost PUT acknowledgement is compared after decryption.
pub struct EncryptedStore {
    inner: Arc<dyn ObjectStore>,
    keys: Vec<Key>,
    domain: String,
}

impl EncryptedStore {
    /// Seals `payload` like a PUT does, then uploads it in parts of
    /// `part_bytes`, for objects too large for one request. Multipart
    /// writes cannot be made conditional here, so unlike `put_opts` with
    /// [`PutMode::Create`](object_store::PutMode::Create) this replaces an
    /// existing object; the caller checks for one first.
    pub async fn put_sealed_in_parts(
        &self,
        path: &ObjectPath,
        payload: PutPayload,
        part_bytes: usize,
    ) -> object_store::Result<()> {
        let key = self
            .keys
            .last()
            .ok_or_else(|| error(anyhow::anyhow!("replica encryption key is missing")))?
            .clone();
        let context = self.context(path);
        let sealed = tokio::task::spawn_blocking(move || seal_payload(&key, &context, payload))
            .await
            .map_err(error)?
            .map_err(error)?;
        let mut upload = self
            .inner
            .put_multipart_opts(path, PutMultipartOptions::default())
            .await?;
        let part_bytes = part_bytes.max(1);
        let uploaded = async {
            let mut offset = 0;
            while offset < sealed.len() {
                let end = (offset + part_bytes).min(sealed.len());
                upload.put_part(sealed.slice(offset..end).into()).await?;
                offset = end;
            }
            upload.complete().await.map(|_| ())
        }
        .await;
        if uploaded.is_err() {
            let _ = upload.abort().await;
        }
        uploaded
    }

    pub fn new(inner: Arc<dyn ObjectStore>, keys: Vec<Key>, domain: String) -> Result<Self> {
        ensure!(!keys.is_empty(), "replica encryption key is missing");
        Ok(Self {
            inner,
            keys,
            domain,
        })
    }

    fn context(&self, path: &ObjectPath) -> Vec<u8> {
        format!("tako replica v1\0{}\0{path}", self.domain).into_bytes()
    }
}

impl fmt::Debug for EncryptedStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptedStore")
            .field("domain", &self.domain)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for EncryptedStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "encrypted:{}:{}", self.domain, self.inner)
    }
}

fn error(error: impl Into<anyhow::Error>) -> object_store::Error {
    object_store::Error::Generic {
        store: "encrypted replica",
        source: error.into().into(),
    }
}

#[async_trait]
impl ObjectStore for EncryptedStore {
    async fn put_opts(
        &self,
        path: &ObjectPath,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        let key = self
            .keys
            .last()
            .ok_or_else(|| error(anyhow::anyhow!("replica encryption key is missing")))?
            .clone();
        let context = self.context(path);
        let bytes = tokio::task::spawn_blocking(move || seal_payload(&key, &context, payload))
            .await
            .map_err(error)?
            .map_err(error)?;
        self.inner.put_opts(path, bytes.into(), options).await
    }

    async fn get_opts(
        &self,
        path: &ObjectPath,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        if options.range.is_some() || options.head {
            return Err(error(anyhow::anyhow!(
                "encrypted replicas require full-object reads"
            )));
        }
        let result = self.inner.get_opts(path, options).await?;
        let mut meta = result.meta.clone();
        let attributes = result.attributes.clone();
        let extensions = result.extensions.clone();
        let temporary = tempfile::tempfile().map_err(error)?;
        let mut output = tokio::fs::File::from_std(temporary);
        let mut stream = result.into_stream();
        while let Some(bytes) = stream.try_next().await? {
            tokio::io::AsyncWriteExt::write_all(&mut output, &bytes)
                .await
                .map_err(error)?;
        }
        tokio::io::AsyncWriteExt::flush(&mut output)
            .await
            .map_err(error)?;
        let file = output.into_std().await;
        let keys = self.keys.clone();
        let context = self.context(path);
        let plaintext = tokio::task::spawn_blocking(move || open_file(&keys, &context, file))
            .await
            .map_err(error)?
            .map_err(error)?;
        meta.size = plaintext.len() as u64;
        Ok(GetResult {
            range: 0..meta.size,
            meta,
            attributes,
            extensions,
            payload: GetResultPayload::Stream(stream::once(async move { Ok(plaintext) }).boxed()),
        })
    }

    fn list(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
        self.inner.delete_stream(paths)
    }

    async fn put_multipart_opts(
        &self,
        _: &ObjectPath,
        _: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        Err(error(anyhow::anyhow!(
            "encrypted replica multipart writes are unsupported"
        )))
    }

    async fn copy_opts(
        &self,
        _: &ObjectPath,
        _: &ObjectPath,
        _: CopyOptions,
    ) -> object_store::Result<()> {
        Err(error(anyhow::anyhow!(
            "encrypted replica objects cannot be copied to another key"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::ObjectStoreExt;

    #[tokio::test]
    async fn encrypted_objects_authenticate_location_and_support_key_rotation() {
        let inner = Arc::new(object_store::memory::InMemory::new());
        let old = Key::new("old".into(), [1; 32]).unwrap();
        let new = Key::new("new".into(), [2; 32]).unwrap();
        let first =
            EncryptedStore::new(inner.clone(), vec![old.clone()], "database-a".into()).unwrap();
        let path = ObjectPath::from("ltx/0/point.ltx");
        first
            .put(&path, "private database contents".into())
            .await
            .unwrap();
        let encrypted = inner.get(&path).await.unwrap().bytes().await.unwrap();
        assert!(!encrypted.windows(7).any(|bytes| bytes == b"private"));
        let rotated =
            EncryptedStore::new(inner.clone(), vec![old, new], "database-a".into()).unwrap();
        assert_eq!(
            rotated.get(&path).await.unwrap().bytes().await.unwrap(),
            "private database contents"
        );
        let other = ObjectPath::from("ltx/0/other.ltx");
        inner.put(&other, encrypted.into()).await.unwrap();
        assert!(rotated.get(&other).await.is_err());
    }

    #[tokio::test]
    async fn streamed_crypto_handles_chunk_boundaries_and_rejects_tampering() {
        let inner = Arc::new(object_store::memory::InMemory::new());
        let key = Key::new("stream".into(), [4; 32]).unwrap();
        let store =
            EncryptedStore::new(inner.clone(), vec![key.clone()], "database".into()).unwrap();
        let path = ObjectPath::from("ltx/0/large.ltx");
        let plaintext: Vec<u8> = (0..(2 * 1024 * 1024 + 17))
            .map(|i| (i % 251) as u8)
            .collect();
        let payload: PutPayload = plaintext
            .chunks(12345)
            .map(Bytes::copy_from_slice)
            .collect();
        store.put(&path, payload).await.unwrap();
        let sealed = inner.get(&path).await.unwrap().bytes().await.unwrap();
        assert_eq!(
            open(std::slice::from_ref(&key), &store.context(&path), &sealed).unwrap(),
            plaintext
        );
        assert_eq!(
            store
                .get(&path)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
                .as_ref(),
            plaintext
        );
        let mut corrupt = sealed.to_vec();
        let middle = corrupt.len() / 2;
        corrupt[middle] ^= 1;
        inner.put(&path, corrupt.into()).await.unwrap();
        assert!(store.get(&path).await.is_err());
        // The small envelope writer and streaming reader use the same format.
        inner
            .put(
                &path,
                seal(&key, &store.context(&path), b"").unwrap().into(),
            )
            .await
            .unwrap();
        assert!(
            store
                .get(&path)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn objects_stop_at_the_aes_gcm_limit() {
        assert!(ensure_sealable(MAX_SEALED_BYTES).is_ok());
        let error = ensure_sealable(MAX_SEALED_BYTES + 1).unwrap_err();
        assert!(error.to_string().contains("AES-GCM limit"), "{error}");
    }

    #[test]
    fn tampered_and_truncated_envelopes_are_rejected() {
        let key = Key::new("key".into(), [3; 32]).unwrap();
        let sealed = seal(&key, b"context", b"database").unwrap();
        for length in 0..sealed.len() {
            assert!(open(std::slice::from_ref(&key), b"context", &sealed[..length]).is_err());
        }
        assert!(open(&[key], b"different context", &sealed).is_err());
    }
}
