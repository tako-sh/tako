mod files;
#[cfg(test)]
mod tests;

use crate::output;
use clap::ValueEnum;
use std::path::Path;

include!(concat!(env!("OUT_DIR"), "/agent_assets.rs"));

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Agent {
    Codex,
    ClaudeCode,
    Cursor,
    GithubCopilot,
}

impl Agent {
    const ALL: [Self; 4] = [
        Self::Codex,
        Self::ClaudeCode,
        Self::Cursor,
        Self::GithubCopilot,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::Cursor => "cursor",
            Self::GithubCopilot => "github-copilot",
        }
    }

    fn skills_dir(self) -> &'static str {
        match self {
            Self::Codex => ".agents/skills",
            Self::ClaudeCode => ".claude/skills",
            Self::Cursor => ".cursor/skills",
            Self::GithubCopilot => ".github/skills",
        }
    }

    fn config(self) -> (&'static str, &'static str) {
        match self {
            Self::Codex => (".codex/config.toml", "mcp_servers"),
            Self::ClaudeCode => (".mcp.json", "mcpServers"),
            Self::Cursor => (".cursor/mcp.json", "mcpServers"),
            Self::GithubCopilot => (".vscode/mcp.json", "servers"),
        }
    }
}

fn detect(project: &Path, home: Option<&Path>) -> Vec<Agent> {
    Agent::ALL
        .into_iter()
        .filter(|agent| {
            let marker = match agent {
                Agent::Codex => ".codex",
                Agent::ClaudeCode => ".claude",
                Agent::Cursor => ".cursor",
                Agent::GithubCopilot => ".vscode",
            };
            project.join(marker).exists()
                || home.is_some_and(|home| home.join(marker).exists())
                || (*agent == Agent::ClaudeCode && project.join(".mcp.json").exists())
        })
        .collect()
}

pub fn run(mut agents: Vec<Agent>, yes: bool, config: Option<&Path>) -> Result<()> {
    let context = super::project_context::resolve(config)?;
    if agents.is_empty() {
        agents = detect(&context.project_dir, dirs::home_dir().as_deref());
    }
    if agents.is_empty() {
        if !output::is_interactive() {
            return Err("No coding agents detected. Select one with --agent codex,claude-code,cursor,github-copilot".into());
        }
        agents.push(output::select(
            "Coding agent",
            None,
            Agent::ALL
                .into_iter()
                .map(|agent| (agent.name().to_string(), agent))
                .collect(),
        )?);
    }
    agents.sort_by_key(|agent| agent.name());
    agents.dedup();
    let executable = std::env::current_exe()?;
    let files = files::plan(
        &context.project_dir,
        &context.config_path,
        &executable,
        &agents,
    )?;
    let paths: Vec<_> = files
        .iter()
        .map(|file| file.path.to_string_lossy().into_owned())
        .collect();
    let names: Vec<_> = agents.iter().map(|agent| agent.name()).collect();
    if !output::is_dry_run() && !yes {
        if !output::is_interactive() {
            return Err("Run tako agents --yes to install skills and configure MCP".into());
        }
        let description = format!(
            "Project-local skills, MCP, and runtime guidance:\n{}",
            paths.join("\n")
        );
        if !output::confirm_with_description(
            &format!("Set up {}?", names.join(", ")),
            Some(&description),
            true,
        )? {
            output::operation_cancelled();
            return Ok(());
        }
    }
    if !output::is_dry_run() {
        for file in &files {
            file.write()?;
        }
        output::success(&format!("Configured {}", names.join(", ")));
        output::hint("Restart your coding agent or reload MCP connections to use Tako.");
    } else {
        for path in &paths {
            output::dry_run_skip(&format!("Write {path}"));
        }
    }
    if output::is_json() {
        output::json_result(
            serde_json::json!({"ok":true,"command":"agents","agents":names,"files":paths,"dry_run":output::is_dry_run()}),
        )?;
    }
    Ok(())
}
