use super::super::LogLevel;
use super::*;
use crate::commands::dev::output_render::{
    extract_repo_slug, fit_scope, format_lan_block, format_log_for_width, format_panel_stacked,
    format_panel_wide, progress_bar, vlen,
};
use crate::output::LOGO_ROWS;
use console::{measure_text_width, strip_ansi_codes, truncate_str};

fn strip_ansi(s: &str) -> String {
    strip_ansi_codes(s).into_owned()
}

mod header_panel;
mod lan;
mod logs;
mod process;
mod repo;
mod terminal;
mod tunnel;
