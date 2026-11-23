//! `tako kv`: Tako-hosted KV stores of JSON values with optional expiry.
//!
//! A KV store is hosted like a SQLite database: on one server of the
//! network, recorded in the network state under a name no database or store
//! shares, and bound per environment, here in `[envs.<env>].kv`.

use std::path::Path;

use clap::{Args, Subcommand};
use tako_core::{Command, DatabaseEngine, KvRequest};

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

    /// Print a key's JSON value
    Get {
        /// Store name
        name: String,
        /// Key
        key: String,
    },

    /// Set a key to a JSON value
    Put {
        /// Store name
        name: String,
        /// Key
        key: String,
        /// JSON value, such as `'{"plan":"pro"}'` or `'"text"'`; `-` reads stdin
        value: String,
        /// Remove the key after this long, such as `30s`, `15m`, `2h` or `7d`
        #[arg(long, value_parser = parse_ttl)]
        ttl: Option<std::time::Duration>,
    },

    /// Remove a key
    #[command(name = "delete-key")]
    DeleteKey {
        /// Store name
        name: String,
        /// Key
        key: String,
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
        KvCommands::Get { name, key } => {
            let value = query(&ctx, &name, KvRequest::Get { key: key.clone() }).await?;
            let Some(json) = value.as_str() else {
                return Err(format!("{key} is not set in {name}.").into());
            };
            if output::is_json() {
                let value: serde_json::Value = serde_json::from_str(json)?;
                return output::json_result(
                    serde_json::json!({"ok": true, "command": "kv get", "key": key, "value": value}),
                );
            }
            println!("{json}");
            Ok(())
        }
        KvCommands::Put {
            name,
            key,
            value,
            ttl,
        } => {
            let json = if value == "-" {
                let mut text = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
                text
            } else {
                value
            };
            let json = json.trim().to_owned();
            if serde_json::from_str::<serde::de::IgnoredAny>(&json).is_err() {
                return Err(
                    "The value must be JSON. Quote text as a JSON string, such as '\"hello\"'."
                        .into(),
                );
            }
            let ttl_ms = ttl.map(|ttl| u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX));
            query(&ctx, &name, KvRequest::Put { key: key.clone(), json, ttl_ms }).await?;
            if output::is_json() {
                return output::json_result(
                    serde_json::json!({"ok": true, "command": "kv put", "key": key}),
                );
            }
            output::success(&format!("Set {key} in {name}"));
            Ok(())
        }
        KvCommands::DeleteKey { name, key } => {
            query(&ctx, &name, KvRequest::Delete { key: key.clone() }).await?;
            if output::is_json() {
                return output::json_result(
                    serde_json::json!({"ok": true, "command": "kv delete-key", "key": key}),
                );
            }
            output::success(&format!("Removed {key} from {name}"));
            Ok(())
        }
        KvCommands::Delete { name, yes } => {
            sqlite::delete(&ctx, &name, yes, DatabaseEngine::Kv).await
        }
    }
}

async fn query(
    ctx: &Context,
    name: &str,
    request: KvRequest,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    ctx.require_admin()?;
    let (_, home) = ctx.resource(name, DatabaseEngine::Kv)?;
    send(
        &home,
        Command::QueryKv {
            name: name.to_owned(),
            request,
        },
    )
    .await
}

/// Parses a TTL: a whole number followed by `s`, `m`, `h` or `d`, or plain
/// seconds.
fn parse_ttl(text: &str) -> Result<std::time::Duration, String> {
    let text = text.trim();
    let (number, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((at, _)) => text.split_at(at),
        None => (text, "s"),
    };
    let seconds_per = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => return Err(format!("unknown unit {unit:?}; use s, m, h or d")),
    };
    let count: u64 = number
        .parse()
        .map_err(|_| format!("{text:?} is not a duration such as 30s, 15m, 2h or 7d"))?;
    match count.checked_mul(seconds_per) {
        Some(0) => Err("the TTL must be longer than zero".into()),
        Some(seconds) => Ok(std::time::Duration::from_secs(seconds)),
        None => Err("the TTL is too long".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttls_parse_in_common_units() {
        use std::time::Duration;
        assert_eq!(parse_ttl("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_ttl("15m"), Ok(Duration::from_secs(900)));
        assert_eq!(parse_ttl("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(parse_ttl("7d"), Ok(Duration::from_secs(604_800)));
        assert_eq!(parse_ttl("45"), Ok(Duration::from_secs(45)));
        assert!(parse_ttl("0s").is_err());
        assert!(parse_ttl("5w").is_err());
        assert!(parse_ttl("h").is_err());
        assert!(parse_ttl("").is_err());
    }
}
