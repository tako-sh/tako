use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;

use crate::engine::inspect_container;
use crate::plan::Plan;
use crate::{
    ComposeRuntime, DependencyCondition, ServiceContainer, UpdateOrder, docker_command,
    engine_output,
};

/// A pending production update. The caller owns commit/rollback; dropping this
/// value does not delete containers. Durable server state and labels make an
/// interrupted update recoverable without executing asynchronous work in Drop.
pub struct Rollout {
    runtime: ComposeRuntime,
    plan: Plan,
    before: Vec<ServiceContainer>,
    selected: Vec<ServiceContainer>,
}

impl ComposeRuntime {
    pub async fn begin_rollout(&self) -> Result<Rollout, String> {
        if self.development {
            return Err("Production rollout requested for a development project".into());
        }
        let (runtime, images) = self.prepared()?;
        let mut model = runtime.model().await?;
        let services = model["services"]
            .as_object_mut()
            .ok_or("Compose has no services")?;
        if images.keys().ne(services.keys()) {
            return Err("Compose service definitions changed since release preparation".into());
        }
        for (name, service) in services {
            let mut command = docker_command(false);
            // Docker can omit Volumes entirely for images without VOLUME.
            // `index` handles both an absent key and an explicit null value.
            command.args([
                "image",
                "inspect",
                "--format",
                r#"{"id":{{json .Id}},"volumes":{{json (index .Config "Volumes")}}}"#,
                &images[name],
            ]);
            let output = engine_output(command, Duration::from_secs(30)).await?;
            let image: Value =
                serde_json::from_slice(&output).map_err(|_| "Invalid prepared image metadata")?;
            let id = image["id"].as_str().ok_or("Prepared image has no ID")?;
            for target in image["volumes"]
                .as_object()
                .into_iter()
                .flat_map(|v| v.keys())
            {
                // Docker matches image VOLUME declarations by exact target. A
                // mount on the parent directory still permits an automatically
                // created anonymous child volume, which replacement would lose.
                let covered = service["volumes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|v| {
                        v["target"].as_str().is_some_and(|mount| {
                            std::path::Path::new(target) == std::path::Path::new(mount)
                        })
                    })
                    || service["tmpfs"].as_array().into_iter().flatten().any(|v| {
                        v.as_str().is_some_and(|mount| {
                            std::path::Path::new(target)
                                == std::path::Path::new(mount.split(':').next().unwrap_or(mount))
                        })
                    });
                if !covered {
                    return Err(format!(
                        "Compose service '{name}': image volume '{target}' needs an explicit named volume or tmpfs mount to avoid losing data on replacement"
                    ));
                }
            }
            service["image"] = Value::String(id.into());
        }
        let plan = Plan::new(model, &runtime)?;
        let before = self.containers().await?;
        Ok(Rollout {
            runtime,
            plan,
            before,
            selected: Vec::new(),
        })
    }
}

impl Rollout {
    pub fn containers(&self) -> &[ServiceContainer] {
        &self.selected
    }

    pub fn ingress(&self) -> Result<&ServiceContainer, String> {
        self.selected
            .iter()
            .find(|c| c.service == self.runtime.config.service)
            .ok_or_else(|| "Compose rollout has no HTTP ingress container".into())
    }

    pub async fn apply(&mut self) -> Result<(), String> {
        if !self.selected.is_empty() {
            return Err("A Compose rollout can only be applied once".into());
        }
        for service in &self.plan.services {
            for (dependency, condition) in &service.dependencies {
                for container in self.selected.iter().filter(|c| &c.service == dependency) {
                    wait_ready(container, Some(*condition), Duration::ZERO).await?;
                }
            }
            for slot in 0..service.replicas {
                let existing = self.runtime.containers().await?;
                let matched = existing.iter().find(|c| {
                    c.service == service.name
                        && c.slot == slot
                        && c.fingerprint == service.fingerprint
                });
                let mut candidate = if let Some(container) = matched {
                    container.clone()
                } else {
                    let model = self.plan.container_model(service, slot, &self.runtime);
                    self.runtime
                        .create_container(model, &service.generation(slot))
                        .await?
                };
                // Create first, even for stop-first, so image/configuration errors
                // cannot unnecessarily stop the predecessor.
                if service.order == UpdateOrder::StopFirst
                    && !candidate.healthy()
                    && !(service.completed && candidate.completed())
                {
                    for old in &existing {
                        let same_slot = old.service == service.name && old.slot == slot;
                        let removed_service =
                            !self.plan.services.iter().any(|p| p.name == old.service);
                        let shares_removed_volume = removed_service
                            && !candidate
                                .writable_volumes
                                .is_disjoint(&old.writable_volumes);
                        if old.id != candidate.id && (same_slot || shares_removed_volume) {
                            self.runtime.stop_container(&old.id).await?;
                        }
                    }
                }
                if !(candidate.running() || (service.completed && candidate.completed())) {
                    self.runtime.start_container(&candidate.id).await?;
                }
                candidate = inspect_container(&candidate.id, false)
                    .await?
                    .ok_or("Compose container disappeared after start")?;
                self.selected.push(candidate);
            }
        }
        // Start consumers according to their declared dependency condition;
        // only then require every service to be ready for the deployment. A
        // service_started dependency must not silently become service_healthy.
        for container in &mut self.selected {
            let service = self
                .plan
                .services
                .iter()
                .find(|s| s.name == container.service)
                .unwrap();
            let condition = service.completed.then_some(DependencyCondition::Completed);
            let was_healthy = self.before.iter().any(|old| {
                old.id == container.id
                    && old.healthy()
                    && old.restart_count == container.restart_count
                    && old.started_at == container.started_at
            });
            let monitor = if was_healthy {
                Duration::ZERO
            } else {
                service.monitor
            };
            *container = wait_ready(container, condition, monitor).await?;
        }
        self.ingress()?;
        Ok(())
    }

    /// Roll back only this attempt's changes. Unchanged containers are neither
    /// stopped nor recreated. A partial failure stays visible to the caller.
    pub async fn rollback(&self) -> Result<(), String> {
        let previous: BTreeSet<_> = self.before.iter().map(|c| c.id.as_str()).collect();
        let mut errors = Vec::new();
        for container in self.runtime.containers().await?.into_iter().rev() {
            if !previous.contains(container.id.as_str()) {
                if let Err(error) = self.runtime.remove_container(&container.id).await {
                    errors.push(error);
                }
            } else if self
                .before
                .iter()
                .any(|old| old.id == container.id && !old.running() && old.state != "restarting")
            {
                if let Err(error) = self.runtime.stop_container(&container.id).await {
                    errors.push(error);
                }
            }
        }
        // Do not restart predecessors while a failed replacement may still be
        // writing their volumes.
        if !errors.is_empty() {
            return Err(errors.join("; "));
        }
        // Service order is dependencies first. The before snapshot also contains
        // removed services, which are restored after the known graph.
        let order: BTreeMap<_, _> = self
            .plan
            .services
            .iter()
            .enumerate()
            .map(|(i, s)| (s.name.as_str(), i))
            .collect();
        let mut before: Vec<_> = self.before.iter().collect();
        before.sort_by_key(|c| order.get(c.service.as_str()).copied().unwrap_or(usize::MAX));
        for old in before {
            if old.running() || old.state == "restarting" {
                match inspect_container(&old.id, false).await? {
                    Some(current) if current.running() => (),
                    Some(_) => {
                        if let Err(error) = self.runtime.start_container(&old.id).await {
                            errors.push(error);
                            continue;
                        }
                        let restored = inspect_container(&old.id, false)
                            .await?
                            .ok_or("Previous Compose container disappeared after restart")?;
                        if let Err(error) =
                            wait_ready(&restored, None, Duration::from_secs(1)).await
                        {
                            errors.push(error);
                        }
                    }
                    None => errors.push(format!(
                        "Previous container for '{}' disappeared during rollback",
                        old.service
                    )),
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    /// Call only after the new release is durable and old HTTP requests drained.
    /// This also reconciles containers left by a terminated server process.
    pub async fn finish(&self) -> Result<(), String> {
        let selected: BTreeSet<_> = self.selected.iter().map(|c| c.id.as_str()).collect();
        let mut errors = Vec::new();
        for container in self.runtime.containers().await? {
            if !selected.contains(container.id.as_str()) {
                if let Err(error) = self.runtime.remove_container(&container.id).await {
                    errors.push(error);
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    pub fn changes(&self) -> Vec<(String, UpdateOrder)> {
        self.plan
            .services
            .iter()
            .filter(|service| {
                (0..service.replicas).any(|slot| {
                    !self.before.iter().any(|c| {
                        c.service == service.name
                            && c.slot == slot
                            && c.fingerprint == service.fingerprint
                            && (c.healthy() || (service.completed && c.completed()))
                    })
                })
            })
            .map(|s| (s.name.clone(), s.order))
            .collect()
    }
}

async fn wait_ready(
    container: &ServiceContainer,
    condition: Option<DependencyCondition>,
    monitor: Duration,
) -> Result<ServiceContainer, String> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(120) + monitor;
    let mut ready_since = None;
    loop {
        let current = inspect_container(&container.id, false)
            .await?
            .ok_or_else(|| format!("Compose service '{}' disappeared", container.service))?;
        // A fast restart can happen entirely between polls. Lifecycle metadata
        // must also stay stable, even when both observations look healthy.
        if current.restart_count != container.restart_count
            || current.started_at != container.started_at
        {
            return Err(format!(
                "Compose service '{}' restarted while waiting for readiness",
                container.service
            ));
        }
        if condition == Some(DependencyCondition::Completed) {
            if current.completed() {
                return Ok(current);
            }
            if current.state == "exited" {
                return Err(format!(
                    "Compose job '{}' exited with status {}",
                    current.service, current.exit_code
                ));
            }
        } else {
            if current.running() && condition == Some(DependencyCondition::Started) {
                return Ok(current);
            }
            if condition == Some(DependencyCondition::Healthy)
                && current.running()
                && current.health.is_empty()
            {
                return Err(format!(
                    "Compose dependency '{}' requires a healthcheck",
                    current.service
                ));
            }
            if current.running()
                && (current.health == "healthy"
                    || (condition.is_none() && current.health.is_empty()))
            {
                if ready_since.get_or_insert_with(Instant::now).elapsed() >= monitor {
                    return Ok(current);
                }
            } else {
                ready_since = None;
            }
            if current.state == "exited" || current.state == "dead" {
                return Err(format!(
                    "Compose service '{}' exited with status {}",
                    current.service, current.exit_code
                ));
            }
            if started.elapsed() >= monitor && matches!(current.health.as_str(), "unhealthy") {
                return Err(format!(
                    "Compose service '{}' did not become healthy",
                    current.service
                ));
            }
            if started.elapsed() >= monitor && current.state == "restarting" {
                return Err(format!(
                    "Compose service '{}' keeps restarting",
                    current.service
                ));
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "Compose service '{}' did not become stable before the readiness deadline",
                current.service
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests;
