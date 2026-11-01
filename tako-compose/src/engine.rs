use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Child;

use crate::plan::{FINGERPRINT_LABEL, RELEASE_LABEL, SERVICE_LABEL, SLOT_LABEL};
use crate::{ComposeRuntime, docker_command, engine_output};

#[derive(Debug, Clone)]
pub struct ServiceContainer {
    pub id: String,
    pub service: String,
    pub generation: String,
    pub slot: u32,
    pub fingerprint: String,
    pub state: String,
    pub health: String,
    pub exit_code: i64,
    pub restart_count: u64,
    pub started_at: String,
    pub port: Option<u16>,
    pub release: Option<PathBuf>,
    pub writable_volumes: BTreeSet<String>,
}

impl ServiceContainer {
    pub fn running(&self) -> bool {
        self.state == "running"
    }
    pub fn healthy(&self) -> bool {
        self.running() && (self.health.is_empty() || self.health == "healthy")
    }
    pub fn completed(&self) -> bool {
        self.state == "exited" && self.exit_code == 0
    }
}

impl ComposeRuntime {
    pub async fn containers(&self) -> Result<Vec<ServiceContainer>, String> {
        project_containers(&self.project, self.development).await
    }

    pub(crate) async fn create_container(
        &self,
        model: Value,
        generation: &str,
    ) -> Result<ServiceContainer, String> {
        let child = self
            .spawn_model(
                &model,
                &[
                    "create",
                    "--no-build",
                    "--pull",
                    "never",
                    "--no-recreate",
                    generation,
                ],
            )
            .await?;
        let output = crate::process_output::capture(
            child,
            Some(Duration::from_secs(120)),
            crate::process_output::StdoutMode::Tail,
        )
        .await
        .map_err(|e| e.to_string())?;
        if !output.status.is_some_and(|s| s.success()) {
            // Compose diagnostics can quote resolved environment values. Keep
            // those out of management errors and server logs.
            return Err(format!(
                "Could not create Compose container for '{generation}'"
            ));
        }
        self.containers()
            .await?
            .into_iter()
            .find(|c| c.generation == generation)
            .ok_or_else(|| format!("Compose did not create container '{generation}'"))
    }

    pub(crate) async fn spawn_model(&self, model: &Value, args: &[&str]) -> Result<Child, String> {
        let mut model = model.clone();
        escape_interpolation(&mut model);
        let bytes = serde_json::to_vec(&model).map_err(|e| e.to_string())?;
        let mut command = docker_command(self.development);
        command
            .envs(&self.env)
            .env("COMPOSE_IGNORE_ORPHANS", "true")
            .current_dir(&self.directory)
            .args([
                "compose",
                "--ansi",
                "never",
                "--project-name",
                &self.project,
                "--project-directory",
            ])
            .arg(&self.directory)
            .args(["--file", "-"])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let mut input = child.stdin.take().ok_or("Missing Compose input pipe")?;
        tokio::time::timeout(Duration::from_secs(30), input.write_all(&bytes))
            .await
            .map_err(|_| "Timed out sending the Compose model")?
            .map_err(|e| e.to_string())?;
        drop(input);
        Ok(child)
    }

    pub async fn logs(&self, containers: &[ServiceContainer]) -> Result<Child, String> {
        // The log client needs only the generation names, not the resolved
        // application environment. Docker supplies each container's logs.
        let services: BTreeMap<_, _> = containers
            .iter()
            .map(|c| (c.generation.clone(), json!({"image":"unused"})))
            .collect();
        self.spawn_model(
            &json!({"services":services}),
            &["logs", "--follow", "--no-color", "--tail", "100"],
        )
        .await
    }

    pub async fn stop_container(&self, id: &str) -> Result<(), String> {
        stop_container(id, self.development).await
    }

    pub(crate) async fn start_container(&self, id: &str) -> Result<(), String> {
        let mut command = docker_command(self.development);
        command.args(["container", "start", id]);
        engine_output(command, Duration::from_secs(60)).await?;
        Ok(())
    }

    pub(crate) async fn remove_container(&self, id: &str) -> Result<(), String> {
        self.stop_container(id).await?;
        if inspect_container(id, self.development).await?.is_some() {
            let mut command = docker_command(self.development);
            // Never remove named volumes. Anonymous image volumes are rejected
            // before creation; -v only cleans incidental anonymous mounts.
            command.args(["container", "rm", "--volumes", id]);
            engine_output(command, Duration::from_secs(60)).await?;
        }
        Ok(())
    }
}

pub(crate) async fn stop_container(id: &str, development: bool) -> Result<(), String> {
    if let Some(container) = inspect_container(id, development).await? {
        if container.running() || container.state == "restarting" || container.state == "paused" {
            let mut command = docker_command(development);
            // Honor Compose stop_grace_period, recorded in the Docker config.
            command.args(["container", "stop", id]);
            engine_output(command, Duration::from_secs(300)).await?;
        }
    }
    Ok(())
}

pub(crate) async fn project_containers(
    project: &str,
    development: bool,
) -> Result<Vec<ServiceContainer>, String> {
    let mut list = docker_command(development);
    list.args([
        "container",
        "ls",
        "--all",
        "--quiet",
        "--no-trunc",
        "--filter",
    ])
    .arg(format!("label=com.docker.compose.project={project}"));
    let ids = engine_output(list, Duration::from_secs(15)).await?;
    let ids = String::from_utf8_lossy(&ids);
    let mut containers = Vec::new();
    for id in ids.split_whitespace() {
        if id.len() != 64 || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err("Docker returned an invalid container ID".into());
        }
        if let Some(container) = inspect_container(id, development).await? {
            containers.push(container);
        }
    }
    containers.sort_by(|a, b| (&a.service, a.slot, &a.id).cmp(&(&b.service, b.slot, &b.id)));
    Ok(containers)
}

pub(crate) async fn inspect_container(
    id: &str,
    development: bool,
) -> Result<Option<ServiceContainer>, String> {
    let mut command = docker_command(development);
    // Restrict inspection output to lifecycle metadata. In particular, do not
    // request Config.Env or health-check output, both of which can hold secrets.
    command.args(["container", "inspect", "--format", r#"{"id":{{json .Id}},"labels":{{json .Config.Labels}},"state":{{json .State.Status}},"health":{{with (index .State "Health")}}{{json .Status}}{{else}}""{{end}},"exit_code":{{json .State.ExitCode}},"restart_count":{{json .RestartCount}},"started_at":{{json .State.StartedAt}},"ports":{{json .NetworkSettings.Ports}},"mounts":{{json .Mounts}}}"#, id]);
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let output = crate::process_output::capture(
        child,
        Some(Duration::from_secs(15)),
        crate::process_output::StdoutMode::Complete,
    )
    .await
    .map_err(|e| e.to_string())?;
    if !output.status.is_some_and(|s| s.success()) {
        if String::from_utf8_lossy(&output.stderr).contains("No such") {
            return Ok(None);
        }
        return Err(format!(
            "Could not inspect Compose container: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let value: Value =
        serde_json::from_slice(&output.stdout).map_err(|_| "Invalid Docker container metadata")?;
    let labels = &value["labels"];
    let generation = labels["com.docker.compose.service"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let port = value["ports"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
        .flat_map(|v| v.as_array().into_iter().flatten())
        .find(|p| p["HostIp"] == "127.0.0.1")
        .and_then(|p| p["HostPort"].as_str())
        .and_then(|p| p.parse().ok());
    Ok(Some(ServiceContainer {
        id: value["id"]
            .as_str()
            .ok_or("Container metadata has no ID")?
            .into(),
        service: labels[SERVICE_LABEL].as_str().unwrap_or(&generation).into(),
        generation,
        slot: labels[SLOT_LABEL]
            .as_str()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        fingerprint: labels[FINGERPRINT_LABEL]
            .as_str()
            .unwrap_or_default()
            .into(),
        state: value["state"].as_str().unwrap_or_default().into(),
        health: value["health"].as_str().unwrap_or_default().into(),
        exit_code: value["exit_code"].as_i64().unwrap_or_default(),
        restart_count: value["restart_count"].as_u64().unwrap_or_default(),
        started_at: value["started_at"].as_str().unwrap_or_default().into(),
        port,
        release: labels[RELEASE_LABEL].as_str().map(PathBuf::from),
        writable_volumes: value["mounts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["Type"] == "volume" && m["RW"] == true)
            .filter_map(|m| m["Name"].as_str().map(str::to_owned))
            .collect(),
    }))
}

fn escape_interpolation(value: &mut Value) {
    match value {
        Value::String(text) => *text = text.replace('$', "$$"),
        Value::Array(values) => values.iter_mut().for_each(escape_interpolation),
        Value::Object(values) => values.values_mut().for_each(escape_interpolation),
        _ => (),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_values_survive_compose_interpolation() {
        let mut model = json!({"services":{"web":{"environment":{"TOKEN":"literal$VALUE/${UNKNOWN}/$$"},"command":["sh", "-c", "echo $TOKEN"]}}});
        escape_interpolation(&mut model);
        assert_eq!(
            model["services"]["web"]["environment"]["TOKEN"],
            "literal$$VALUE/$${UNKNOWN}/$$$$"
        );
        assert_eq!(model["services"]["web"]["command"][2], "echo $$TOKEN");
    }
}
