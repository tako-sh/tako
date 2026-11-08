use super::model::{Manifest, invalid};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

const LIMIT: usize = 4 * 1024 * 1024;
const LOCK_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Lock {
    version: u32,
    schemas: BTreeMap<String, Entry>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    sha256: String,
    schema: Manifest,
}

pub struct Resolved {
    pub manifests: Vec<Manifest>,
    lock_path: PathBuf,
    lock: Option<String>,
}

impl Resolved {
    pub fn commit(&self) -> io::Result<bool> {
        self.lock.as_ref().map_or(Ok(false), |body| {
            super::write_generated(&self.lock_path, body)
        })
    }
}

/// The committed lock embeds remote schemas, so generation also works in an
/// offline clean checkout. A refresh is explicit; local files are always reread.
pub fn resolve(project: &Path, sources: &[String], update: bool) -> io::Result<Resolved> {
    let lock_path = project.join("tako-schema.lock");
    let mut previous: Lock = if lock_path.is_file() {
        if fs::metadata(&lock_path)?.len() > LOCK_LIMIT {
            return Err(invalid("schema lock is too large"));
        }
        let lock: Lock = serde_json::from_slice(&fs::read(&lock_path)?)?;
        if lock.version != 1 {
            return Err(invalid("unsupported schema lock version"));
        }
        lock
    } else {
        Lock {
            version: 1,
            ..Default::default()
        }
    };
    let mut next = Lock {
        version: 1,
        ..Default::default()
    };
    let mut manifests = Vec::new();
    let mut apps = std::collections::BTreeSet::new();
    for source in sources {
        let remote =
            reqwest::Url::parse(source).is_ok_and(|url| matches!(url.scheme(), "http" | "https"));
        let schema = if remote {
            let pinned = previous.schemas.remove(source);
            let schema = if !update && let Some(entry) = pinned {
                if hash(&entry.schema)? != entry.sha256 {
                    return Err(invalid(format!(
                        "schema lock content hash mismatch for {source}"
                    )));
                }
                entry.schema
            } else {
                let bytes = fetch(source)?;
                serde_json::from_slice::<Manifest>(&bytes)
                    .map_err(|error| invalid(format!("invalid schema from {source}: {error}")))?
            };
            next.schemas.insert(
                source.clone(),
                Entry {
                    sha256: hash(&schema)?,
                    schema: schema.clone(),
                },
            );
            schema
        } else {
            if source.contains(':') {
                return Err(invalid("schema sources must be file paths or HTTP(S) URLs"));
            }
            let path = project.join(source);
            if fs::metadata(&path)?.len() > LIMIT as u64 {
                return Err(invalid("schema file is too large"));
            }
            serde_json::from_slice::<Manifest>(&fs::read(&path)?)
                .map_err(|error| invalid(format!("invalid schema {}: {error}", path.display())))?
        };
        schema.validate()?;
        if !apps.insert(schema.tako.app.clone()) {
            return Err(invalid(format!(
                "duplicate schema for app {}",
                schema.tako.app
            )));
        }
        manifests.push(schema);
    }
    manifests.sort_by(|a, b| a.tako.app.cmp(&b.tako.app));
    let lock = if !next.schemas.is_empty() || lock_path.exists() {
        Some(serde_json::to_string_pretty(&next)? + "\n")
    } else {
        None
    };
    if lock.as_ref().is_some_and(|s| s.len() as u64 > LOCK_LIMIT) {
        return Err(invalid("schema lock is too large"));
    }
    Ok(Resolved {
        manifests,
        lock_path,
        lock,
    })
}

fn hash(schema: &Manifest) -> io::Result<String> {
    // Value canonicalizes object key order independently of exporter formatting.
    Ok(
        Sha256::digest(serde_json::to_vec(&serde_json::to_value(schema)?)?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

fn fetch(source: &str) -> io::Result<Vec<u8>> {
    let source = source.to_string();
    // Generation is shared with synchronous build hooks inside an async CLI.
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async move {
                let url =
                    reqwest::Url::parse(&source).map_err(|_| invalid("invalid schema URL"))?;
                if !url.username().is_empty()
                    || url.password().is_some()
                    || url.fragment().is_some()
                {
                    return Err(invalid(
                        "schema URLs cannot contain credentials or fragments",
                    ));
                }
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .redirect(reqwest::redirect::Policy::custom(|attempt| {
                        if attempt.previous().len() > 5
                            || !matches!(attempt.url().scheme(), "http" | "https")
                            || (attempt.previous().iter().any(|u| u.scheme() == "https")
                                && attempt.url().scheme() != "https")
                        {
                            attempt.error("invalid schema redirect")
                        } else {
                            attempt.follow()
                        }
                    }))
                    .build()
                    .map_err(|_| invalid("cannot create schema HTTP client"))?;
                let mut response = client
                    .get(url)
                    .send()
                    .await
                    .and_then(reqwest::Response::error_for_status)
                    .map_err(|_| invalid(format!("cannot fetch schema {source}")))?;
                let mut bytes = Vec::new();
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|_| invalid("cannot read schema response"))?
                {
                    if bytes.len() + chunk.len() > LIMIT {
                        return Err(invalid("schema response exceeds 4 MiB"));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(bytes)
            })
    })
    .join()
    .map_err(|_| invalid("schema fetch failed"))?
}
