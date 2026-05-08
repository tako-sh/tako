use std::path::Path;
use std::sync::Arc;

use russh::keys::{Algorithm, Error as KeyError, PrivateKeyWithHashAlg, load_secret_key};

use super::*;

impl SshClient {
    pub(super) async fn authenticate(&mut self, handle: &mut Handle<SshHandler>) -> SshResult<()> {
        let keys_dir = self.config.keys_directory();

        // id_dsa is obsolete (OpenSSH dropped support in 7.0) — don't try it.
        let key_names = ["id_ed25519", "id_rsa", "id_ecdsa"];

        let mut last_error = None;
        let mut found_any_key_file = false;

        for key_name in &key_names {
            let key_path = keys_dir.join(key_name);

            if !key_path.exists() {
                continue;
            }

            found_any_key_file = true;

            match self.try_key_auth(handle, &key_path).await {
                Ok(Some(public_key)) => {
                    self.authenticated_public_key = Some(public_key);
                    return Ok(());
                }
                Ok(None) => {
                    tracing::trace!("Key not accepted ({key_name})");
                }
                Err(e) => {
                    tracing::trace!("Key auth failed ({key_name}): {e}");
                    last_error = Some(e);
                }
            }
        }

        match self.try_agent_auth(handle).await {
            Ok(Some(public_key)) => {
                self.authenticated_public_key = Some(public_key);
                return Ok(());
            }
            Ok(None) => {}
            Err(e) => last_error = Some(e),
        }

        if found_any_key_file {
            Err(last_error.unwrap_or_else(|| {
                SshError::Authentication("No SSH keys were accepted by the server".to_string())
            }))
        } else {
            Err(last_error.unwrap_or(SshError::NoKeysFound(keys_dir)))
        }
    }

    async fn try_agent_auth(&self, handle: &mut Handle<SshHandler>) -> SshResult<Option<String>> {
        #[cfg(unix)]
        {
            use russh::keys::agent::client::AgentClient;

            let mut agent = match AgentClient::connect_env().await {
                Ok(agent) => agent,
                Err(_) => return Ok(None),
            };

            let keys = agent.request_identities().await.map_err(|e| {
                SshError::Authentication(format!("ssh-agent identities failed: {e}"))
            })?;

            for identity in keys {
                let public_key = identity.public_key().into_owned();
                match handle
                    .authenticate_publickey_with(
                        self.config.user.as_str(),
                        public_key.clone(),
                        None,
                        &mut agent,
                    )
                    .await
                {
                    Ok(result) if result.success() => {
                        return public_key
                            .to_openssh()
                            .map(Some)
                            .map_err(|e| SshError::Authentication(e.to_string()));
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        return Err(SshError::Authentication(format!(
                            "ssh-agent authentication failed: {e}"
                        )));
                    }
                }
            }

            Ok(None)
        }

        #[cfg(not(unix))]
        {
            let _ = handle;
            Ok(None)
        }
    }

    async fn try_key_auth(
        &self,
        handle: &mut Handle<SshHandler>,
        key_path: &Path,
    ) -> SshResult<Option<String>> {
        let key = match load_secret_key(key_path, None) {
            Ok(k) => k,
            Err(KeyError::KeyIsEncrypted) => match self
                .config
                .key_passphrase
                .clone()
                .or_else(|| crate::ssh::key_passphrase_for_path(key_path))
            {
                Some(pass) => {
                    load_secret_key(key_path, Some(&pass)).map_err(|e| SshError::KeyLoad {
                        path: key_path.to_path_buf(),
                        reason: e.to_string(),
                    })?
                }
                None => {
                    return Err(SshError::KeyLoad {
                        path: key_path.to_path_buf(),
                        reason: KeyError::KeyIsEncrypted.to_string(),
                    });
                }
            },
            Err(e) => {
                return Err(SshError::KeyLoad {
                    path: key_path.to_path_buf(),
                    reason: e.to_string(),
                });
            }
        };

        let hash_alg = if matches!(key.algorithm(), Algorithm::Rsa { .. }) {
            handle
                .best_supported_rsa_hash()
                .await
                .map_err(|e| SshError::Authentication(e.to_string()))?
                .flatten()
        } else {
            None
        };

        let auth_result = handle
            .authenticate_publickey(
                self.config.user.as_str(),
                PrivateKeyWithHashAlg::new(Arc::new(key.clone()), hash_alg),
            )
            .await
            .map_err(|e| SshError::Authentication(e.to_string()))?;

        if !auth_result.success() {
            return Ok(None);
        }

        key.public_key()
            .to_openssh()
            .map(Some)
            .map_err(|e| SshError::Authentication(e.to_string()))
    }
}
