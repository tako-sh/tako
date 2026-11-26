//! Project checks, run when doctor starts inside a project. Each is cheap and
//! reads local state, `tako.toml`, or one network pull; a check that needs an
//! unreachable network is skipped with a note instead of failing doctor.

use serde::Serialize;
use serde_json::{Value, json};

use crate::commands::project_resources::{
    self, Access, EnvResources, ProjectResources, missing_cloudflare_token,
};

/// How long before expiry a value is reported.
const EXPIRY_DAYS: i64 = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CheckStatus {
    Ok,
    Warning,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Check {
    pub id: &'static str,
    pub label: &'static str,
    pub status: CheckStatus,
    pub message: String,
}

pub(super) struct ProjectReport {
    pub app: String,
    pub checks: Vec<Check>,
}

/// Run the project checks for the project in or named by `config`. Outside
/// a project there is nothing to check.
pub(super) async fn gather(config: Option<&std::path::Path>) -> Option<ProjectReport> {
    let context = crate::commands::project_context::resolve_optional(config).ok()??;
    let inventory = crate::commands::helpers::load_management_servers(None)
        .await
        .unwrap_or_default();
    let project =
        match tokio::task::block_in_place(|| project_resources::load(&context, &inventory)) {
            Ok(project) => project,
            Err(error) => {
                return Some(ProjectReport {
                    app: context.project_dir.display().to_string(),
                    checks: vec![Check {
                        id: "project",
                        label: "Project",
                        status: CheckStatus::Skipped,
                        message: format!("Could not read the project: {error}"),
                    }],
                });
            }
        };
    let checks = tokio::task::block_in_place(|| {
        checks(&project, |env| {
            project_resources::missing_key_grants(env, &project.app, &inventory)
        })
    });
    Some(ProjectReport {
        app: project.app.clone(),
        checks,
    })
}

/// Findings of one check across environments.
#[derive(Default)]
struct Findings {
    warnings: Vec<String>,
    skipped: Vec<String>,
}

impl Findings {
    fn into_check(self, id: &'static str, label: &'static str, ok: &str) -> Check {
        let (status, message) = if !self.warnings.is_empty() {
            let mut message = self.warnings.join("; ");
            if !self.skipped.is_empty() {
                message.push_str(&format!(" (skipped: {})", self.skipped.join("; ")));
            }
            (CheckStatus::Warning, message)
        } else if !self.skipped.is_empty() {
            (CheckStatus::Skipped, self.skipped.join("; "))
        } else {
            (CheckStatus::Ok, ok.to_string())
        };
        Check {
            id,
            label,
            status,
            message,
        }
    }
}

/// Deployed environments: those with a network.
fn deployed(project: &ProjectResources) -> impl Iterator<Item = &EnvResources> {
    project.envs.iter().filter(|env| env.network_id.is_some())
}

fn unreadable_note(env: &EnvResources) -> String {
    match env.access {
        Access::Deploy => format!("{}: secrets need a full client", env.name),
        _ => format!("{}: this device is not a member of its network", env.name),
    }
}

pub(super) fn checks(
    project: &ProjectResources,
    key_grants: impl Fn(&EnvResources) -> Result<Vec<(String, usize)>, String>,
) -> Vec<Check> {
    let mut ssl = Findings::default();
    let mut expiring = Findings::default();
    let mut databases = Findings::default();
    let mut grants = Findings::default();

    for env in deployed(project) {
        if !env.can_read_secrets() {
            ssl.skipped.push(unreadable_note(env));
            grants.skipped.push(unreadable_note(env));
        } else {
            if missing_cloudflare_token(&project.config, &project.secrets, &env.name) {
                ssl.warnings.push(format!(
                    "{env}: its routes need ssl.cloudflare; run `tako credential set ssl.cloudflare --env {env}`",
                    env = env.name
                ));
            }
            if !env.servers.is_empty() {
                match key_grants(env) {
                    Ok(missing) => {
                        for (server, count) in missing {
                            grants.warnings.push(format!(
                                "{}: {server} lacks keys for {count} current secret record{}; they arrive in the background and deploys to it wait for them",
                                env.name,
                                if count == 1 { "" } else { "s" }
                            ));
                        }
                    }
                    Err(error) => grants
                        .skipped
                        .push(format!("{}: network unreachable ({error})", env.name)),
                }
            }
        }

        if !env.databases.is_empty() || !env.kv.is_empty() {
            match env
                .network_id
                .as_ref()
                .and_then(|id| project.databases.get(id))
            {
                Some(Err(error)) => databases
                    .skipped
                    .push(format!("{}: network unreachable ({error})", env.name)),
                None => databases
                    .skipped
                    .push(format!("{}: no local network profile", env.name)),
                Some(Ok(_)) => {
                    for binding in env.databases.iter().filter(|db| db.generation.is_none()) {
                        databases.warnings.push(format!(
                            "{}: {} uses {}, which has no network record; create it with `tako db create {}`",
                            env.name, binding.usage.label(), binding.database, binding.database
                        ));
                    }
                    for binding in env.kv.iter().filter(|db| db.generation.is_none()) {
                        databases.warnings.push(format!(
                            "{}: {} binds KV store {}, which has no network record. Deploy with full access on a single-server network or place it with `tako kv create {} --server <server>`",
                            env.name, binding.usage.label(), binding.database, binding.database
                        ));
                    }
                }
            }
        }
    }

    for env in project.envs.iter().filter(|env| env.can_read_secrets()) {
        for (name, expired) in
            project_resources::expiring_values(&project.secrets, &env.name, EXPIRY_DAYS)
        {
            expiring.warnings.push(if expired {
                format!("{}: {name} expired", env.name)
            } else {
                format!("{}: {name} expires within {EXPIRY_DAYS} days", env.name)
            });
        }
    }
    for env in deployed(project).filter(|env| !env.can_read_secrets()) {
        expiring.skipped.push(unreadable_note(env));
    }

    vec![
        ssl.into_check(
            "wildcard_ssl",
            "Wildcard SSL",
            "Routes that need ssl.cloudflare have it",
        ),
        expiring.into_check(
            "expiring_values",
            "Expiring values",
            &format!("No secret or credential expires within {EXPIRY_DAYS} days"),
        ),
        databases.into_check(
            "database_records",
            "Database records",
            "Every bound database has a network record",
        ),
        grants.into_check(
            "key_grants",
            "Key grants",
            "Every target server holds keys for current secrets",
        ),
    ]
}

pub(super) fn format(buf: &mut Vec<String>, report: &ProjectReport) {
    super::common::heading(buf, &format!("Project {}", report.app));
    let labels: Vec<&str> = report.checks.iter().map(|check| check.label).collect();
    let width = super::common::label_width(&labels);
    for check in &report.checks {
        let value = match check.status {
            CheckStatus::Ok => crate::output::theme_success(&check.message).to_string(),
            CheckStatus::Warning => crate::output::theme_warning(&check.message).to_string(),
            CheckStatus::Skipped => crate::output::theme_muted(&check.message).to_string(),
        };
        super::common::row(buf, check.label, &value, width);
    }
}

pub(super) fn json(report: Option<&ProjectReport>) -> Value {
    match report {
        None => Value::Null,
        Some(report) => json!({
            "app": report.app,
            "checks": report.checks.iter().map(|check| json!({
                "name": check.id,
                "status": check.status,
                "message": check.message,
            })).collect::<Vec<_>>(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::project_resources::{DatabaseUsage, EnvResources, ProjectDatabase};
    use crate::config::{EncryptedSecretValue, SecretsStore, TakoToml};

    const NETWORK: &str = "0123456789abcdef0123456789abcdef";

    fn env(name: &str, access: Access, generation: Option<&str>) -> EnvResources {
        EnvResources {
            name: name.into(),
            network: Some("main".into()),
            network_id: Some(NETWORK.into()),
            access,
            servers: vec!["edge-a".into(), "edge-b".into()],
            secrets: None,
            credentials: None,
            storages: vec![],
            databases: vec![ProjectDatabase {
                usage: DatabaseUsage::Binding {
                    binding: "main".into(),
                },
                database: "orders".into(),
                generation: generation.map(str::to_owned),
                leader: None,
            }],
            kv: vec![],
            note: None,
        }
    }

    fn project(envs: Vec<EnvResources>, records: Result<(), String>) -> ProjectResources {
        let config = TakoToml::parse(
            "name = \"shop\"\n[envs.production]\nroute = \"*.shop.example.com\"\n[envs.staging]\nroute = \"staging.shop.example.com\"\n",
        )
        .unwrap();
        let mut secrets = SecretsStore::default();
        secrets.ensure_env_key_id("production").unwrap();
        secrets
            .environments
            .get_mut("production")
            .unwrap()
            .app
            .insert(
                "OLD".into(),
                EncryptedSecretValue::new("ciphertext".into(), Some("2001-01-01".into())),
            );
        ProjectResources {
            app: "shop".into(),
            config,
            secrets,
            envs,
            databases: [(NETWORK.to_string(), records.map(|()| Vec::new()))].into(),
        }
    }

    fn by_id<'a>(checks: &'a [Check], id: &str) -> &'a Check {
        checks.iter().find(|check| check.id == id).unwrap()
    }

    #[test]
    fn a_full_client_gets_a_warning_from_each_failing_check() {
        let project = project(vec![env("production", Access::Full, None)], Ok(()));
        let checks = checks(&project, |_| Ok(vec![("edge-b".into(), 2)]));
        let ssl = by_id(&checks, "wildcard_ssl");
        assert_eq!(ssl.status, CheckStatus::Warning);
        assert!(
            ssl.message
                .contains("tako credential set ssl.cloudflare --env production")
        );
        let expiring = by_id(&checks, "expiring_values");
        assert_eq!(expiring.status, CheckStatus::Warning);
        assert_eq!(expiring.message, "production: OLD expired");
        let databases = by_id(&checks, "database_records");
        assert_eq!(databases.status, CheckStatus::Warning);
        assert!(databases.message.contains("tako db create orders"));
        let grants = by_id(&checks, "key_grants");
        assert_eq!(grants.status, CheckStatus::Warning);
        assert!(
            grants
                .message
                .contains("edge-b lacks keys for 2 current secret records")
        );
    }

    #[test]
    fn healthy_environments_pass_every_check() {
        let mut project = project(vec![env("staging", Access::Full, Some("edge-a"))], Ok(()));
        project.secrets = SecretsStore::default();
        let checks = checks(&project, |_| Ok(vec![]));
        assert!(
            checks.iter().all(|check| check.status == CheckStatus::Ok),
            "{checks:?}"
        );
    }

    #[test]
    fn an_unreachable_network_and_a_deploy_client_skip_with_a_note() {
        let project = project(
            vec![env("production", Access::Deploy, None)],
            Err("connection refused".into()),
        );
        let checks = checks(&project, |_| {
            panic!("deploy clients can't read secret records")
        });
        for id in ["wildcard_ssl", "expiring_values", "key_grants"] {
            let check = by_id(&checks, id);
            assert_eq!(check.status, CheckStatus::Skipped, "{id}");
            assert_eq!(check.message, "production: secrets need a full client");
        }
        let databases = by_id(&checks, "database_records");
        assert_eq!(databases.status, CheckStatus::Skipped);
        assert_eq!(
            databases.message,
            "production: network unreachable (connection refused)"
        );
        let grants_down = checks_with_unreachable_grants();
        assert_eq!(grants_down.status, CheckStatus::Skipped);
        assert!(
            grants_down
                .message
                .contains("network unreachable (timed out)")
        );
    }

    fn checks_with_unreachable_grants() -> Check {
        let project = project(vec![env("staging", Access::Full, Some("edge-a"))], Ok(()));
        by_id(&checks(&project, |_| Err("timed out".into())), "key_grants").clone()
    }

    #[test]
    fn json_names_each_check_and_its_status() {
        let report = ProjectReport {
            app: "shop".into(),
            checks: vec![Check {
                id: "key_grants",
                label: "Key grants",
                status: CheckStatus::Skipped,
                message: "staging: network unreachable (timed out)".into(),
            }],
        };
        assert_eq!(
            json(Some(&report)),
            json!({"app":"shop","checks":[{"name":"key_grants","status":"skipped","message":"staging: network unreachable (timed out)"}]})
        );
        assert_eq!(json(None), Value::Null);
    }
}
