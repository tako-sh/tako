//! CLI Integration Tests
//!
//! Tests the full tako CLI workflow from init to deploy using mock servers.

#[path = "cli_integration/support.rs"]
mod support;

#[path = "cli_integration/deploy_command.rs"]
mod deploy_command;
#[path = "cli_integration/dev_daemon_commands.rs"]
mod dev_daemon_commands;
#[path = "cli_integration/help_and_version.rs"]
mod help_and_version;
#[path = "cli_integration/init.rs"]
mod init;
#[path = "cli_integration/output_modes.rs"]
mod output_modes;
#[path = "cli_integration/secret_commands.rs"]
mod secret_commands;
#[path = "cli_integration/server_commands.rs"]
mod server_commands;
#[path = "cli_integration/status_command.rs"]
mod status_command;
#[path = "cli_integration/uninstall_commands.rs"]
mod uninstall_commands;
