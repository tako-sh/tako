use crate::cli::Cli;
use clap::{ArgAction, Command, CommandFactory};
use rmcp::model::{Tool, ToolAnnotations};
use serde_json::{Map, Value, json};
use std::{any::TypeId, collections::BTreeMap};

#[derive(Clone)]
pub(super) struct CommandTool {
    pub tool: Tool,
    pub path: Vec<String>,
    command: Command,
}

const SKIP: &[&str] = &[
    "help",
    "version",
    "ci",
    "json",
    "verbose",
    "config",
    "background",
];

pub(super) fn catalog() -> BTreeMap<String, CommandTool> {
    let mut cli = Cli::command();
    cli.build();
    let mut tools = BTreeMap::new();
    for command in cli.get_subcommands() {
        if [
            "init",
            "agents",
            "mcp",
            "completions",
            "upgrade",
            "uninstall",
            "help",
        ]
        .contains(&command.get_name())
        {
            continue;
        }
        visit(command, vec![command.get_name().to_string()], &mut tools);
    }
    tools
}

fn visit(command: &Command, path: Vec<String>, tools: &mut BTreeMap<String, CommandTool>) {
    if !command.is_subcommand_required_set() {
        let name = format!("tako_{}", path.join("_").replace('-', "_"));
        let read_only = matches!(path[0].as_str(), "doctor" | "status" | "logs" | "version")
            || path
                .last()
                .is_some_and(|last| matches!(last.as_str(), "list" | "status"));
        let description = format!(
            "{}\nUses the configured Tako project and CLI credentials. {}{}",
            command
                .get_about()
                .map(ToString::to_string)
                .unwrap_or_default(),
            if read_only {
                "Read-only inspection."
            } else {
                "May change local or remote state. Establish user authorization before use; yes skips CLI confirmation."
            },
            if path == ["dev"] {
                " Starts development and returns at readiness, leaving the app running. Use tako_dev_stop to stop it."
            } else if path == ["run"] {
                " Executes local code with app secrets; does not execute on deployment servers."
            } else {
                ""
            }
        );
        let tool = Tool::new(name.clone(), description, schema(command, &path)).with_annotations(
            ToolAnnotations::new()
                .read_only(read_only)
                .destructive(!read_only)
                .open_world(true),
        );
        tools.insert(
            name,
            CommandTool {
                tool,
                path: path.clone(),
                command: command.clone(),
            },
        );
    }
    for child in command
        .get_subcommands()
        .filter(|child| !child.is_hide_set() && child.get_name() != "help")
    {
        let mut path = path.clone();
        path.push(child.get_name().to_string());
        visit(child, path, tools);
    }
}

fn exposed(argument: &clap::Arg, path: &[String]) -> bool {
    !SKIP.contains(&argument.get_id().as_str())
        && !argument.is_hide_set()
        && argument.get_id() != "tail"
        && (argument.get_id() != "dry_run" || supports_dry_run(path))
}

fn supports_dry_run(path: &[String]) -> bool {
    matches!(path[0].as_str(), "deploy" | "delete")
        || (path[0] == "servers"
            && path
                .last()
                .is_some_and(|name| matches!(name.as_str(), "add" | "rm")))
        || (path[0] == "backups"
            && path
                .last()
                .is_some_and(|name| matches!(name.as_str(), "now" | "download" | "restore")))
}

fn integer(argument: &clap::Arg) -> bool {
    let id = argument.get_value_parser().type_id();
    [
        TypeId::of::<u8>(),
        TypeId::of::<u16>(),
        TypeId::of::<u32>(),
        TypeId::of::<u64>(),
        TypeId::of::<usize>(),
        TypeId::of::<i32>(),
        TypeId::of::<i64>(),
    ]
    .into_iter()
    .any(|other| id == other)
}

fn multiple(argument: &clap::Arg) -> bool {
    matches!(argument.get_action(), ArgAction::Append)
        || argument
            .get_num_args()
            .is_some_and(|range| range.max_values() > 1)
}

fn schema(command: &Command, path: &[String]) -> Map<String, Value> {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for argument in command
        .get_arguments()
        .filter(|argument| exposed(argument, path))
    {
        let name = argument.get_id().as_str();
        let mut property = if matches!(
            argument.get_action(),
            ArgAction::SetTrue | ArgAction::SetFalse
        ) {
            json!({"type":"boolean"})
        } else if integer(argument) {
            json!({"type":"integer"})
        } else {
            json!({"type":"string"})
        };
        if let Some(values) = argument.get_value_parser().possible_values() {
            property["enum"] = json!(
                values
                    .filter(|value| !value.is_hide_set())
                    .map(|value| value.get_name().to_string())
                    .collect::<Vec<_>>()
            );
        }
        if multiple(argument) {
            property = json!({"type":"array","items":property});
        }
        property["description"] = json!(
            argument
                .get_help()
                .map(ToString::to_string)
                .unwrap_or_default()
        );
        if argument.is_required_set() {
            required.push(name.to_string());
        }
        properties.insert(name.to_string(), property);
    }
    properties.insert("stdin".into(), json!({"type":"string","description":"Input for non-interactive secret/credential setters or scripts. Never place secret values in command arguments."}));
    properties.insert("timeout_seconds".into(), json!({"type":"integer","minimum":1,"maximum":1800,"default":120,"description":"Maximum local execution time. Remote work may continue after timeout; inspect status before retrying."}));
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}).as_object().unwrap().clone()
}

impl CommandTool {
    pub fn arguments(&self, input: &Map<String, Value>) -> Result<Vec<String>, String> {
        for name in input.keys() {
            if !self.tool.input_schema["properties"]
                .as_object()
                .unwrap()
                .contains_key(name)
            {
                return Err(format!("Unknown argument: {name}"));
            }
        }
        let mut arguments = self.path.clone();
        let mut positionals = Vec::new();
        for argument in self
            .command
            .get_arguments()
            .filter(|argument| exposed(argument, &self.path))
        {
            let name = argument.get_id().as_str();
            let Some(value) = input.get(name) else {
                if argument.is_required_set() {
                    return Err(format!("Missing required argument: {name}"));
                }
                continue;
            };
            if matches!(
                argument.get_action(),
                ArgAction::SetTrue | ArgAction::SetFalse
            ) {
                let enabled = value
                    .as_bool()
                    .ok_or_else(|| format!("{name} must be boolean"))?;
                if enabled {
                    arguments.push(format!("--{}", argument.get_long().unwrap()));
                }
                continue;
            }
            let values: Vec<&Value> = if multiple(argument) {
                value
                    .as_array()
                    .ok_or_else(|| format!("{name} must be an array"))?
                    .iter()
                    .collect()
            } else {
                vec![value]
            };
            let mut rendered = Vec::new();
            for value in values {
                rendered.push(if integer(argument) {
                    value
                        .as_i64()
                        .map(|value| value.to_string())
                        .ok_or_else(|| format!("{name} must be an integer"))?
                } else {
                    value
                        .as_str()
                        .ok_or_else(|| format!("{name} must be a string"))?
                        .to_string()
                });
            }
            if let Some(long) = argument.get_long() {
                for value in rendered {
                    arguments.push(format!("--{long}={value}"));
                }
            } else {
                positionals.push((argument.get_index().unwrap_or(0), rendered));
            }
        }
        positionals.sort_by_key(|(index, _)| *index);
        if positionals.iter().any(|(_, values)| !values.is_empty()) {
            arguments.push("--".into());
        }
        for (_, values) in positionals {
            arguments.extend(values);
        }
        if self.path == ["dev"] {
            arguments.push("--background".into());
        }
        // Clap remains the source of truth for ranges, enums, and conflicts.
        let mut validation = vec!["tako".to_string()];
        validation.extend(arguments.clone());
        Cli::try_parse_from(validation).map_err(|error| error.to_string())?;
        Ok(arguments)
    }
}

use clap::Parser;
