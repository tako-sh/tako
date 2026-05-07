mod cli;
mod commands;
mod dev_server_client;
mod github;
mod keychain;
mod output;
mod paths;
pub mod shell;
mod ui;

// Internal modules (moved from tako-core)
pub mod app;
pub mod build;
pub mod config;
pub mod crypto;
pub mod dev;
pub mod ssh;
pub mod validation;

use clap::Parser;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

use cli::Cli;

fn main() {
    // Parse CLI arguments early so we can configure logging/output.
    let cli = Cli::parse();

    crate::output::set_verbose(cli.verbose);
    crate::output::set_ci(cli.ci);
    crate::output::set_dry_run(cli.dry_run);

    // Hide cursor for the entire process lifetime when running in interactive
    // pretty mode. Individual prompts (text fields) temporarily show it while
    // the user is typing. The Ctrl-C handler and exit paths restore it.
    if crate::output::is_pretty() && crate::output::is_interactive() {
        crate::output::set_cursor_globally_hidden();
    }

    ctrlc::set_handler(|| {
        crate::output::restore_cursor();
        crate::output::clear_interrupt_output();
        let handled_in_ui = crate::ui::interrupt_with_message(crate::output::OPERATION_CANCELLED);
        let finalized_ui = crate::ui::finalize_active_session();
        if !handled_in_ui && !finalized_ui {
            crate::ui::cleanup_on_interrupt();
            crate::output::operation_cancelled();
        }
        std::process::exit(130);
    })
    .expect("failed to install Ctrl-C handler");

    // Tracing subscriber: only installed in verbose/CI mode.
    // In normal mode, tracing calls are no-ops (no subscriber).
    if cli.verbose || cli.ci {
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_writer(std::io::stderr)
            .event_format(output::ScopeFormat);

        tracing_subscriber::registry()
            .with(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("tako=trace,rustls=off,warn")),
            )
            .with(output::ScopeLayer)
            .with(fmt_layer)
            .init();
    }

    // Run the command
    if let Err(e) = cli.run() {
        crate::output::restore_cursor();
        if crate::output::is_operation_cancelled_error(e.as_ref()) {
            let handled_in_ui =
                crate::ui::interrupt_with_message(crate::output::OPERATION_CANCELLED);
            let finalized_ui = crate::ui::finalize_active_session();
            if !handled_in_ui && !finalized_ui {
                crate::output::operation_cancelled();
            }
            std::process::exit(130);
        }
        let _ = crate::ui::finalize_active_session();
        if !crate::output::is_silent_exit_error(e.as_ref()) {
            crate::output::error_stderr(&e.to_string());
        }
        std::process::exit(1);
    }

    crate::output::restore_cursor();
}
