//! `tako kv`: Tako-hosted KV stores, which speak the Redis protocol.
//!
//! A KV store is hosted like a SQLite database: on one server of the
//! network, recorded in the network state under a name no database or store
//! shares, and bound per environment, here in `[envs.<env>].kv`.

use std::io::{BufRead, Write};
use std::path::Path;

use clap::{Args, Subcommand};
use tako_core::{Command, DatabaseEngine};

use crate::commands::sqlite::{self, Context, send};
use crate::output;

#[derive(Args)]
pub struct KvArgs {
    #[command(subcommand)]
    command: KvCommands,
}

#[derive(Subcommand)]
pub enum KvCommands {
    /// Create a KV store on a server of the network
    Create {
        /// Store name, unique within the network
        name: String,
        /// Server that hosts the store (required when the network has several)
        #[arg(long)]
        server: Option<String>,
    },

    /// List the network's KV stores
    #[command(visible_alias = "ls")]
    List,

    /// Show a store's size and the apps that use it
    Status {
        /// Store name (every store the project binds when omitted)
        name: Option<String>,
    },

    /// Run Redis commands against a store
    Shell {
        /// Store name
        name: String,
        /// One command to run, such as `GET session:42`, instead of opening
        /// an interactive shell
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },

    /// Delete a store and every key in it
    #[command(visible_alias = "rm")]
    Delete {
        /// Store name
        name: String,
        /// Skip the confirmation
        #[arg(long, short)]
        yes: bool,
    },
}

pub fn run(args: KvArgs, config_path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(args, config_path))
}

async fn run_async(
    args: KvArgs,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let network = crate::network::network_flag();
    let ctx = Context::load(config_path, network).await?;
    match args.command {
        KvCommands::Create { name, server } => {
            sqlite::create(&ctx, &name, server.as_deref(), DatabaseEngine::Kv).await
        }
        KvCommands::List => sqlite::list(&ctx, DatabaseEngine::Kv),
        KvCommands::Status { name } => {
            sqlite::status(&ctx, name.as_deref(), DatabaseEngine::Kv).await
        }
        KvCommands::Shell { name, command } => {
            ctx.require_admin()?;
            let (_, home) = ctx.resource(&name, DatabaseEngine::Kv)?;
            shell(&home, &name, command).await
        }
        KvCommands::Delete { name, yes } => {
            sqlite::delete(&ctx, &name, yes, DatabaseEngine::Kv).await
        }
    }
}

async fn shell(
    home: &sqlite::target::Home,
    name: &str,
    command: Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    if !command.is_empty() {
        let reply = query(home, name, command).await?;
        // An error reply fails the command; with --json the CLI reports it
        // as `{"ok": false, "error": ...}`.
        if let Some(error) = reply.get("error").and_then(|e| e.as_str()) {
            if !output::is_json() {
                println!("{}", render(&reply, 0));
            }
            return Err(error.to_owned().into());
        }
        if output::is_json() {
            return output::json_result(
                serde_json::json!({"ok": true, "command": "kv", "reply": reply}),
            );
        }
        println!("{}", render(&reply, 0));
        return Ok(());
    }
    let interactive = output::is_interactive();
    if interactive {
        output::hint(&format!(
            "Connected to {name} on {}. Enter Redis commands; `quit` ends the shell.",
            home.name
        ));
    }
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        if interactive {
            eprint!("{name}> ");
            std::io::stderr().flush()?;
        }
        let Some(line) = lines.next() else { break };
        let args = match split(&line?) {
            Ok(args) => args,
            Err(error) => {
                output::error_stderr(&error);
                continue;
            }
        };
        match args.first().map(|a| a.to_ascii_lowercase()) {
            None => continue,
            Some(first) if first == "quit" || first == "exit" => break,
            Some(_) => {}
        }
        match query(home, name, args).await {
            Ok(reply) if output::is_json() => println!("{}", serde_json::to_string(&reply)?),
            Ok(reply) => println!("{}", render(&reply, 0)),
            Err(error) => output::error_stderr(&error.to_string()),
        }
    }
    Ok(())
}

async fn query(
    home: &sqlite::target::Home,
    name: &str,
    args: Vec<String>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    send(
        home,
        Command::QueryKv {
            name: name.to_owned(),
            args,
        },
    )
    .await
}

/// Renders a reply the way `redis-cli` does in a terminal.
fn render(reply: &serde_json::Value, indent: usize) -> String {
    use serde_json::Value;
    match reply {
        Value::Null => "(nil)".into(),
        Value::String(text) => text.clone(),
        Value::Number(number) => format!("(integer) {number}"),
        Value::Bool(value) => value.to_string(),
        Value::Object(object) => match (object.get("error"), object.get("base64")) {
            (Some(Value::String(error)), _) => format!("(error) {error}"),
            (_, Some(Value::String(base64))) => format!("(binary, {} bytes)", base64.len() * 3 / 4),
            _ => reply.to_string(),
        },
        Value::Array(items) if items.is_empty() => "(empty array)".into(),
        Value::Array(items) => {
            let width = items.len().to_string().len();
            items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let prefix = format!("{:>width$}) ", i + 1);
                    let pad = if i == 0 { 0 } else { indent };
                    format!(
                        "{}{prefix}{}",
                        " ".repeat(pad),
                        render(item, indent + prefix.len())
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

/// Splits a shell line into arguments: on whitespace, with double quotes
/// (supporting `\n`, `\"` and `\\` escapes) and single quotes.
fn split(line: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else {
            return Ok(args);
        };
        let mut arg = String::new();
        if first == '"' || first == '\'' {
            chars.next();
            loop {
                match chars.next() {
                    None => return Err("Unbalanced quotes".into()),
                    Some(c) if c == first => break,
                    Some('\\') if first == '"' => match chars.next() {
                        Some('n') => arg.push('\n'),
                        Some('r') => arg.push('\r'),
                        Some('t') => arg.push('\t'),
                        Some(other) => arg.push(other),
                        None => return Err("Unbalanced quotes".into()),
                    },
                    Some(c) => arg.push(c),
                }
            }
            if chars.peek().is_some_and(|c| !c.is_whitespace()) {
                return Err("Closing quote must be followed by a space".into());
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                arg.push(c);
                chars.next();
            }
        }
        args.push(arg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lines_split_like_redis_cli() {
        assert_eq!(
            split(r#"SET "user:1 name" 'it''s' "a\"b\n""#).unwrap_err(),
            "Closing quote must be followed by a space"
        );
        assert_eq!(
            split(r#"  SET "user:1 name" 'x y' "a\"b\n"  "#).unwrap(),
            vec!["SET", "user:1 name", "x y", "a\"b\n"]
        );
        assert!(split("GET \"open").is_err());
        assert!(split("   ").unwrap().is_empty());
    }

    #[test]
    fn replies_render_like_redis_cli() {
        assert_eq!(render(&json!("OK"), 0), "OK");
        assert_eq!(render(&json!(5), 0), "(integer) 5");
        assert_eq!(render(&json!(null), 0), "(nil)");
        assert_eq!(render(&json!([]), 0), "(empty array)");
        assert_eq!(render(&json!({"error": "ERR x"}), 0), "(error) ERR x");
        assert_eq!(render(&json!({"base64": "AAAA"}), 0), "(binary, 3 bytes)");
        assert_eq!(
            render(&json!(["0", ["a", "b"]]), 0),
            "1) 0\n2) 1) a\n   2) b"
        );
        assert_eq!(render(&json!([["f", "v"]]), 0), "1) 1) f\n   2) v");
    }
}
