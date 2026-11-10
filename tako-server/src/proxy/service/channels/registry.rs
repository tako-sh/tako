use super::*;

impl TakoProxy {
    /// The app's channel backend from runtime credentials, resolved once per
    /// credential generation rather than on every channel request.
    pub(super) async fn channel_postgres_url_for(
        &self,
        app_name: &str,
        native_configured: bool,
    ) -> Result<Option<String>> {
        let generation = self
            .channel_credentials_generation
            .load(std::sync::atomic::Ordering::Acquire);
        if let Some((resolved_at, url)) = self.channel_credentials.read().get(app_name)
            && *resolved_at == generation
        {
            return Ok(url.clone());
        }
        let resolver = self.channel_postgres_url.read().clone();
        let app_id = app_name.to_owned();
        let postgres_url = tokio::task::spawn_blocking(move || match resolver {
            Some(resolve) => resolve(&app_id),
            None if native_configured => {
                Err("native channel credentials resolver missing".to_string())
            }
            None => Ok(None),
        })
        .await
        .map_err(|_| {
            Error::explain(
                ErrorType::InternalError,
                "channel credential resolver failed",
            )
        })?
        .map_err(|_| Error::explain(ErrorType::InternalError, "channel credentials unavailable"))?;
        // Tagged with the generation read before resolving, so a deployment that
        // lands in between forces one more resolution instead of going stale.
        self.channel_credentials
            .write()
            .insert(app_name.to_owned(), (generation, postgres_url.clone()));
        Ok(postgres_url)
    }

    /// Keep the reader and its broker subscription in the existing per-app cache.
    /// A reader opened against a backend that has since changed is dropped and
    /// reopened once, so credential rotations recover without a restart.
    pub(super) async fn channel_store_for_app(
        &self,
        app_name: &str,
    ) -> Result<Arc<ProxyChannelStore>> {
        let native = self.native_channels.read().clone();
        let native_configured = native.is_some();
        let postgres_url = self
            .channel_postgres_url_for(app_name, native_configured)
            .await?;
        for reopened in [false, true] {
            let cell = self
                .channel_stores
                .write()
                .entry(app_name.to_owned())
                .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new()))
                .clone();
            let store = cell
                .get_or_try_init(|| async {
                    let app = app_scope(app_name).map_err(|_| {
                        Error::explain(ErrorType::InternalError, "invalid channel app scope")
                    })?;
                    let config = crate::channels::app_channel_store_config_with_postgres(
                        self.lb.app_manager().data_dir(),
                        app_name,
                        postgres_url.as_deref(),
                    );
                    ProxyChannelStore::open(config, app, native.clone())
                        .await
                        .map(Arc::new)
                        .map_err(|_| {
                            Error::explain(
                                ErrorType::InternalError,
                                "channel reader or native broker unavailable",
                            )
                        })
                })
                .await?;
            let current = store.is_native() == native_configured
                && store.check_backend(postgres_url.as_deref()).is_ok();
            if !current {
                let mut stores = self.channel_stores.write();
                if stores
                    .get(app_name)
                    .is_some_and(|cached| Arc::ptr_eq(cached, &cell))
                {
                    stores.remove(app_name);
                }
                drop(stores);
                // Requests still holding the stale reader fail closed instead of
                // letting its reconnects re-register the old backend.
                store.detach();
                if reopened {
                    return Err(Error::explain(
                        ErrorType::InternalError,
                        "channel backend configuration changed; reopen app channel reader",
                    ));
                }
                continue;
            }
            store.ensure_ready().await.map_err(|_| {
                Error::explain(
                    ErrorType::InternalError,
                    "native channel broker unavailable",
                )
            })?;
            return Ok(store.clone());
        }
        unreachable!("channel reader is reopened at most once")
    }

    pub(super) async fn channel_meta_for_app(
        &self,
        app_name: &str,
        instance: &crate::instances::Instance,
        channel: &str,
    ) -> std::result::Result<ChannelDefinitionMeta, ChannelError> {
        self.channel_registry
            .get_or_load(app_name, instance.build_version(), channel, || async {
                let defs = fetch_channel_registry(app_name, instance).await?;
                let store = self
                    .channel_store_for_app(app_name)
                    .await
                    .map_err(|error| ChannelError::Storage(error.to_string()))?;
                for definition in &defs {
                    store
                        .sync_channel_async(&definition.channel, &definition.lifecycle_auth())
                        .await?;
                }
                Ok(defs)
            })
            .await
    }
}
