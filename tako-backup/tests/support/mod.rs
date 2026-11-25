pub mod capture;
mod fixture;
use std::{
    fmt,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result as StoreResult,
    path::Path as ObjectPath,
};

pub struct Process {
    child: Child,
    input: ChildStdin,
    output: mpsc::Receiver<String>,
}

impl Process {
    pub fn start(mode: &str, database: &Path, spool: &Path) -> Result<Self> {
        let mut child = Command::new(std::env::current_exe()?)
            .args(["--exact", "support::fixture::subprocess", "--nocapture"])
            .env("TAKO_LTX_FIXTURE_MODE", mode)
            .env("TAKO_LTX_FIXTURE_DB", database)
            .env("TAKO_LTX_FIXTURE_SPOOL", spool)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let input = child.stdin.take().context("child stdin")?;
        let stdout = child.stdout.take().context("child stdout")?;
        let (sender, output) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Some((_, message)) = line.split_once("LTX-FIXTURE>") else {
                    continue;
                };
                if sender.send(message.to_owned()).is_err() {
                    break;
                }
            }
        });
        let process = Self {
            child,
            input,
            output,
        };
        ensure!(process.receive()? == "ready", "fixture did not start");
        Ok(process)
    }

    pub fn send(&mut self, command: &str) -> Result<()> {
        writeln!(self.input, "{command}")?;
        self.input.flush()?;
        Ok(())
    }

    pub fn receive(&self) -> Result<String> {
        self.output
            .recv_timeout(Duration::from_secs(10))
            .context("fixture response")
    }

    pub fn request(&mut self, command: &str) -> Result<String> {
        self.send(command)?;
        self.receive()
    }

    pub fn crash(&mut self) -> Result<()> {
        self.child.kill()?;
        self.child.wait()?;
        Ok(())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug, Default)]
pub struct Calls {
    pub puts: AtomicUsize,
    pub gets: AtomicUsize,
    pub lists: AtomicUsize,
    pub deletes: AtomicUsize,
    pub fail_delete_at: AtomicUsize,
    pub fail_before: AtomicBool,
    pub lose_ack: AtomicBool,
}

#[derive(Debug)]
pub struct Remote {
    inner: Arc<dyn ObjectStore>,
    pub calls: Arc<Calls>,
}

impl Remote {
    pub fn new(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)?;
        Ok(Self {
            inner: Arc::new(tako_storage::local(path).map_err(anyhow::Error::msg)?),
            calls: Arc::default(),
        })
    }
}

impl fmt::Display for Remote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

fn fault(flag: &AtomicBool) -> bool {
    flag.swap(false, Ordering::SeqCst)
}

fn upload_error() -> object_store::Error {
    object_store::Error::Generic {
        store: "injected",
        source: std::io::Error::other("upload failed").into(),
    }
}

#[async_trait]
impl ObjectStore for Remote {
    async fn put_opts(
        &self,
        path: &ObjectPath,
        payload: PutPayload,
        options: PutOptions,
    ) -> StoreResult<PutResult> {
        self.calls.puts.fetch_add(1, Ordering::SeqCst);
        if fault(&self.calls.fail_before) {
            return Err(upload_error());
        }
        let result = self.inner.put_opts(path, payload, options).await?;
        if fault(&self.calls.lose_ack) {
            return Err(upload_error());
        }
        Ok(result)
    }

    async fn get_opts(&self, path: &ObjectPath, options: GetOptions) -> StoreResult<GetResult> {
        self.calls.gets.fetch_add(1, Ordering::SeqCst);
        self.inner.get_opts(path, options).await
    }

    fn list(&self, prefix: Option<&ObjectPath>) -> BoxStream<'static, StoreResult<ObjectMeta>> {
        self.calls.lists.fetch_add(1, Ordering::SeqCst);
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&ObjectPath>) -> StoreResult<ListResult> {
        self.calls.lists.fetch_add(1, Ordering::SeqCst);
        self.inner.list_with_delimiter(prefix).await
    }

    async fn put_multipart_opts(
        &self,
        path: &ObjectPath,
        options: PutMultipartOptions,
    ) -> StoreResult<Box<dyn MultipartUpload>> {
        self.calls.puts.fetch_add(1, Ordering::SeqCst);
        let inner = self.inner.put_multipart_opts(path, options).await?;
        Ok(Box::new(Upload {
            inner,
            calls: Arc::clone(&self.calls),
        }))
    }

    fn delete_stream(
        &self,
        paths: BoxStream<'static, StoreResult<ObjectPath>>,
    ) -> BoxStream<'static, StoreResult<ObjectPath>> {
        let calls = Arc::clone(&self.calls);
        self.inner
            .delete_stream(paths)
            .map(move |result| {
                let count = calls.deletes.fetch_add(1, Ordering::SeqCst) + 1;
                if calls.fail_delete_at.load(Ordering::SeqCst) == count {
                    return Err(upload_error());
                }
                result
            })
            .boxed()
    }

    async fn copy_opts(
        &self,
        from: &ObjectPath,
        to: &ObjectPath,
        options: CopyOptions,
    ) -> StoreResult<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// Counts parts and can lose the acknowledgement of a completed upload.
#[derive(Debug)]
struct Upload {
    inner: Box<dyn MultipartUpload>,
    calls: Arc<Calls>,
}

#[async_trait]
impl MultipartUpload for Upload {
    fn put_part(&mut self, data: PutPayload) -> object_store::UploadPart {
        self.calls.puts.fetch_add(1, Ordering::SeqCst);
        self.inner.put_part(data)
    }

    async fn complete(&mut self) -> StoreResult<PutResult> {
        let result = self.inner.complete().await?;
        if fault(&self.calls.lose_ack) {
            return Err(upload_error());
        }
        Ok(result)
    }

    async fn abort(&mut self) -> StoreResult<()> {
        self.inner.abort().await
    }
}
