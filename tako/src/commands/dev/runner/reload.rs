use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc, watch};

use super::super::{
    ScopedLog, compute_dev_env, compute_dev_hosts, inject_dev_allowed_hosts, inject_dev_data_dir,
    inject_dev_secrets, load_dev_tako_toml, watcher,
};

pub(super) struct ConfigReloadLoop {
    pub project_dir: PathBuf,
    pub config_path: PathBuf,
    pub config_key: String,
    pub app_name: String,
    pub variant: Option<String>,
    pub domain: String,
    pub base_domain: Option<String>,
    pub env_state: Arc<Mutex<HashMap<String, String>>>,
    pub hosts_state: Arc<Mutex<Vec<String>>>,
    pub command: Vec<String>,
    pub log_tx: mpsc::Sender<ScopedLog>,
    pub should_exit_tx: watch::Sender<bool>,
    pub readiness_failure_hint: Option<String>,
    pub worker_command: Option<Vec<String>>,
}

pub(super) fn spawn_config_reload_loop(
    ctx: ConfigReloadLoop,
    mut cfg_rx: mpsc::Receiver<watcher::WatchChange>,
) {
    tokio::spawn(async move {
        while let Some(change) = cfg_rx.recv().await {
            if !ctx.config_path.exists() {
                let _ = ctx
                    .log_tx
                    .send(ScopedLog::error(
                        "tako",
                        format!(
                            "{} was removed — stopping dev server",
                            ctx.config_path.display()
                        ),
                    ))
                    .await;
                let _ = ctx.should_exit_tx.send(true);
                return;
            }
            if change == watcher::WatchChange::GeneratedDeclarations {
                if let Err(err) = crate::build::js::write_generated_files(&ctx.project_dir) {
                    let _ = ctx
                        .log_tx
                        .send(ScopedLog::warn(
                            "tako",
                            format!("Failed to regenerate tako.d.ts: {err}"),
                        ))
                        .await;
                }
                continue;
            }

            let cfg = match load_dev_tako_toml(&ctx.config_path) {
                Ok(c) => c,
                Err(e) => {
                    let _ = ctx
                        .log_tx
                        .send(ScopedLog::error("tako", format!("tako.toml error: {}", e)))
                        .await;
                    continue;
                }
            };

            for warning in cfg.ignored_reserved_var_warnings() {
                let _ = ctx
                    .log_tx
                    .send(ScopedLog::warn("tako", format!("Validation: {}", warning)))
                    .await;
            }

            let new_hosts = match compute_dev_hosts(
                &ctx.app_name,
                &cfg,
                &ctx.domain,
                ctx.base_domain.as_deref(),
            ) {
                Ok(hosts) => hosts,
                Err(msg) => {
                    let _ = ctx
                        .log_tx
                        .send(ScopedLog::error(
                            "tako",
                            format!("tako.toml invalid routes: {}", msg),
                        ))
                        .await;
                    continue;
                }
            };

            let mut new_env = compute_dev_env(&cfg);
            if crate::build::detect_build_adapter(&ctx.project_dir).preset_group()
                == crate::build::PresetGroup::Js
            {
                new_env.insert("TAKO_APP_ROOT".to_string(), cfg.js_app_root().to_string());
            }
            inject_dev_allowed_hosts(&new_hosts, &mut new_env);
            if let Err(msg) = inject_dev_data_dir(&ctx.project_dir, &mut new_env) {
                let _ = ctx
                    .log_tx
                    .send(ScopedLog::error(
                        "tako",
                        format!("Failed to prepare TAKO_DATA_DIR: {msg}"),
                    ))
                    .await;
                continue;
            }

            if let Err(msg) =
                inject_dev_secrets(&ctx.project_dir, &mut new_env).map_err(|e| e.to_string())
            {
                let _ = ctx
                    .log_tx
                    .send(ScopedLog::warn(
                        "tako",
                        format!("Failed to reload secrets: {}", msg),
                    ))
                    .await;
            }

            let _ = crate::build::js::write_generated_files_for_adapter_and_app_root(
                &ctx.project_dir,
                crate::build::detect_build_adapter(&ctx.project_dir),
                cfg.js_app_root(),
            );

            *ctx.env_state.lock().await = new_env.clone();
            let hosts_changed = {
                let mut cur = ctx.hosts_state.lock().await;
                let changed = *cur != new_hosts;
                *cur = new_hosts.clone();
                changed
            };

            let should_register = hosts_changed || matches!(change, watcher::WatchChange::Config);
            if should_register {
                register_updated_app(&ctx, &cfg, &new_hosts, &new_env).await;
            } else {
                restart_app_for_change(&ctx, change).await;
            }
        }
    });
}

async fn register_updated_app(
    ctx: &ConfigReloadLoop,
    cfg: &crate::config::TakoToml,
    new_hosts: &[String],
    new_env: &HashMap<String, String>,
) {
    let storages = match super::load_dev_storages(&ctx.project_dir).map_err(|e| e.to_string()) {
        Ok(storages) => storages,
        Err(msg) => {
            let _ = ctx
                .log_tx
                .send(ScopedLog::warn(
                    "tako",
                    format!("Failed to reload storages: {msg}"),
                ))
                .await;
            HashMap::new()
        }
    };
    let project_dir_display = ctx.project_dir.to_string_lossy();
    let reg_result =
        crate::dev_server_client::register_app(crate::dev_server_client::RegisterAppRequest {
            config_path: &ctx.config_key,
            project_dir: &project_dir_display,
            app_name: &ctx.app_name,
            variant: ctx.variant.as_deref(),
            hosts: new_hosts,
            command: &ctx.command,
            env: new_env,
            images: &cfg.images,
            storages: &storages,
            readiness_failure_hint: ctx.readiness_failure_hint.as_deref(),
            worker_command: ctx.worker_command.as_deref(),
        })
        .await
        .map_err(|e| e.to_string());
    if let Err(msg) = reg_result {
        let _ = ctx
            .log_tx
            .send(ScopedLog::warn(
                "tako",
                format!("failed to update routing: {}", msg),
            ))
            .await;
    }
}

async fn restart_app_for_change(ctx: &ConfigReloadLoop, change: watcher::WatchChange) {
    let restart_reason = match change {
        watcher::WatchChange::Config => "tako.toml changed, restarting…",
        watcher::WatchChange::Secrets => "Secrets changed, restarting…",
        watcher::WatchChange::Channels => "channels/ changed, restarting…",
        watcher::WatchChange::Workflows => "workflows/ changed, restarting…",
        watcher::WatchChange::GeneratedDeclarations => unreachable!(),
    };
    let _ = ctx
        .log_tx
        .send(ScopedLog::info("tako", restart_reason))
        .await;
    let _ = crate::dev_server_client::restart_app(&ctx.config_key).await;
}
