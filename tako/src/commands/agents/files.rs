use super::{ASSETS, Agent, Result};
use jsonc_parser::cst::{CstInputValue, CstRootNode};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub(super) struct File {
    pub path: PathBuf,
    content: String,
}

impl File {
    pub fn write(&self) -> Result<()> {
        let parent = self.path.parent().unwrap();
        fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        if let Ok(metadata) = fs::metadata(&self.path) {
            temporary
                .as_file()
                .set_permissions(metadata.permissions())?;
        }
        temporary.write_all(self.content.as_bytes())?;
        temporary.persist(&self.path)?;
        Ok(())
    }
}

fn read(project: &Path, path: &Path) -> Result<String> {
    // Never follow project-local links while rewriting agent instructions/config.
    let mut ancestor = Some(path);
    while let Some(path) = ancestor {
        if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(format!("Agent setup will not overwrite symlink {}. Remove the link or configure this client manually", path.display()).into());
        }
        if path == project {
            break;
        }
        ancestor = path.parent();
    }
    match fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(format!("Could not read {}: {error}", path.display()).into()),
    }
}

const START: &str = "<!-- tako:agents:start -->";
const END: &str = "<!-- tako:agents:end -->";

fn instructions(existing: &str, config: &Path) -> Result<String> {
    let guidance = format!(
        r#"{START}
## Tako

Use Tako for development, deployment, diagnostics, logs, secrets, and server operations.
Read the installed `tako` skill and the matching language SDK skill before working on this app.
The Tako MCP server provides tools and bundled docs; start with `tako_docs` or `tako_search_docs`.
Use `{}` as this app's configuration.
Inspect running apps before starting development, and verify using the Tako-provided URL.
Check status and logs after a deploy. Keep secret values out of logs and reports.
Without MCP, use `tako <command> --help` and `--json`; docs: https://tako.sh/llms.txt.
Refresh skills and MCP with `tako agents`.
{END}"#,
        config.display()
    );
    match (existing.find(START), existing.find(END)) {
        (Some(start), Some(end)) if start < end => {
            if existing[start + START.len()..].contains(START) || existing[end + END.len()..].contains(END) {
                return Err("Multiple Tako instruction sections found; keep one marked section before running tako agents".into());
            }
            Ok(format!("{}{}{}", &existing[..start], guidance, &existing[end + END.len()..]))
        }
        (None, None) => {
            let separator = if existing.is_empty() || existing.ends_with("\n\n") {
                ""
            } else if existing.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            Ok(format!("{existing}{separator}{guidance}\n"))
        }
        _ => Err("Incomplete Tako instruction section; repair its tako:agents markers before running tako agents".into()),
    }
}

pub(super) fn plan(
    project: &Path,
    config: &Path,
    executable: &Path,
    agents: &[Agent],
) -> Result<Vec<File>> {
    let mut files = Vec::new();
    let args = [
        "mcp".to_string(),
        "--config".to_string(),
        config.to_string_lossy().into_owned(),
    ];
    for agent in agents {
        let (name, section) = agent.config();
        let path = project.join(name);
        let existing = read(project, &path)?;
        let content = if *agent == Agent::Codex {
            let mut document: toml_edit::DocumentMut = existing
                .parse()
                .map_err(|error| format!("Invalid {}: {error}", path.display()))?;
            if document
                .get(section)
                .is_some_and(|item| !item.is_table_like())
            {
                return Err(format!("{section} in {} must be a table", path.display()).into());
            }
            if document.get(section).is_none() {
                let mut table = toml_edit::Table::new();
                table.set_implicit(true);
                document[section] = toml_edit::Item::Table(table);
            }
            if document[section]
                .get("tako")
                .is_some_and(|item| !item.is_table_like())
            {
                return Err(format!("tako in {} must be a table", path.display()).into());
            }
            if document[section].get("tako").is_none() {
                document[section]["tako"] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            let entry = &mut document[section]["tako"];
            entry["command"] = toml_edit::value(executable.to_string_lossy().as_ref());
            entry["args"] = toml_edit::value(args.iter().cloned().collect::<toml_edit::Array>());
            entry["startup_timeout_sec"] = toml_edit::value(30);
            entry["tool_timeout_sec"] = toml_edit::value(1800);
            document.to_string()
        } else {
            let document = CstRootNode::parse(
                if existing.trim().is_empty() {
                    "{}"
                } else {
                    &existing
                },
                &Default::default(),
            )
            .map_err(|error| format!("Invalid {}: {error}", path.display()))?;
            let object = document
                .object_value()
                .ok_or_else(|| format!("{} must contain a JSON object", path.display()))?;
            let servers = object
                .object_value_or_create(section)
                .ok_or_else(|| format!("{section} in {} must be an object", path.display()))?;
            let mut properties = vec![
                (
                    "command".to_string(),
                    CstInputValue::String(executable.to_string_lossy().into_owned()),
                ),
                (
                    "args".to_string(),
                    CstInputValue::Array(args.iter().cloned().map(CstInputValue::String).collect()),
                ),
            ];
            if *agent == Agent::GithubCopilot {
                properties.push((
                    "type".to_string(),
                    CstInputValue::String("stdio".to_string()),
                ));
            }
            let entry = servers
                .object_value_or_create("tako")
                .ok_or_else(|| format!("tako in {} must be an object", path.display()))?;
            for (name, value) in properties {
                if let Some(property) = entry.get(&name) {
                    property.set_value(value);
                } else {
                    entry.append(&name, value);
                }
            }
            let mut content = document.to_string();
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content
        };
        files.push(File { path, content });
        for (key, content) in ASSETS.iter().filter(|(key, _)| key.starts_with("skills/")) {
            let name = key.strip_prefix("skills/").unwrap();
            let directory = project.join(agent.skills_dir()).join(name);
            let path = if fs::symlink_metadata(&directory)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                // skills.sh links client skills to .agents/skills in the same
                // project. Keep those links and refresh only that owned target.
                let canonical = fs::canonicalize(project)?.join(".agents/skills").join(name);
                if fs::canonicalize(&directory)? != canonical {
                    return Err(format!("Agent setup will not overwrite skill link {}. Only links to this project's .agents/skills/{name} can be refreshed", directory.display()).into());
                }
                project.join(".agents/skills").join(name).join("SKILL.md")
            } else {
                directory.join("SKILL.md")
            };
            read(project, &path)?;
            if !files.iter().any(|file| file.path == path) {
                files.push(File {
                    path,
                    content: content.to_string(),
                });
            }
        }
    }
    for name in ["AGENTS.md", "CLAUDE.md"]
        .into_iter()
        .filter(|name| *name != "CLAUDE.md" || agents.contains(&Agent::ClaudeCode))
    {
        let path = project.join(name);
        let existing = read(project, &path)?;
        let relative_config = config.strip_prefix(project).unwrap_or(config);
        files.push(File {
            path,
            content: instructions(&existing, relative_config)?,
        });
    }
    Ok(files)
}
