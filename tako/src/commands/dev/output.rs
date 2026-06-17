//! Streaming dev output for `tako dev`.
//!
//! Prints a branded header once at startup, then streams logs above a sticky
//! footer. The footer (bordered panel + right-aligned keymap) is erased and
//! reprinted below every log line so it stays pinned at the bottom.
//! No alternate screen — native terminal scrollback and search work normally.

use std::io::{self, Write};
use std::time::Duration;

use crossterm::cursor;
use crossterm::event::{Event, KeyCode, KeyModifiers};
use crossterm::terminal::{self, ClearType, SetTitle};
use crossterm::{execute, queue};
use sysinfo::{Pid, ProcessesToUpdate, System};
use tokio::sync::mpsc;

use super::output_render::{
    DIM, RESET, ShareRowState, ShareRows, format_header, format_keymap, format_lan_block,
    format_log, format_panel, format_tunnel_block, git_info, to_local_route,
};
use super::{DevEvent, LogLevel, ScopedLog, TunnelCloseReason};

const METRICS_REFRESH_SECS: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlCmd {
    Restart,
    Terminate,
    ToggleLan,
    ToggleTunnel,
}

/// Exit value returned by [`run_dev_output`].
pub enum DevOutputExit {
    /// The client terminated (Ctrl+C or supervisor-driven exit).
    Terminate,
    /// The user pressed `b` to background the app while keeping it running.
    ///
    /// The caller receives the log/event receivers so it can keep draining
    /// them to the JSONL store while the process stays alive in the background.
    Disconnect {
        #[allow(dead_code)]
        log_rx: mpsc::Receiver<ScopedLog>,
        #[allow(dead_code)]
        event_rx: mpsc::Receiver<DevEvent>,
    },
}

// ── Process metrics ───────────────────────────────────────────────────────────

fn collect_process_tree_pids(processes: &[(Pid, Option<Pid>)], root: Pid) -> Vec<Pid> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    let mut seen = std::collections::HashSet::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        out.push(pid);
        for (child_pid, parent) in processes {
            if *parent == Some(pid) {
                stack.push(*child_pid);
            }
        }
    }
    out
}

fn process_tree_metrics(sys: &System, root: Pid) -> Option<(f32, u64)> {
    sys.process(root)?;
    let index: Vec<(Pid, Option<Pid>)> = sys
        .processes()
        .iter()
        .map(|(p, pr)| (*p, pr.parent()))
        .collect();
    let pids = collect_process_tree_pids(&index, root);
    let mut cpu = 0.0_f32;
    let mut mem = 0u64;
    for pid in pids {
        if let Some(p) = sys.process(pid) {
            cpu += p.cpu_usage();
            mem += p.memory();
        }
    }
    Some((cpu, mem))
}

fn tunnel_close_log(close_reason: Option<TunnelCloseReason>) -> ScopedLog {
    let (level, message) = close_reason
        .map(|reason| (reason.log_level(), reason.log_message()))
        .unwrap_or((LogLevel::Info, "Tunnel off"));
    ScopedLog::at(level, "tako", message)
}

fn tunnel_connection_log(connected: bool) -> ScopedLog {
    if connected {
        ScopedLog::info("tako", "Tunnel reconnected")
    } else {
        ScopedLog::warn("tako", "Tunnel reconnecting: connection lost")
    }
}

// ── Sticky footer ─────────────────────────────────────────────────────────────

struct StickyFooter {
    lines: Vec<String>,
    /// Terminal width when the footer was last drawn, used to compute the
    /// maximum number of wrapped visual rows on resize.
    drawn_cols: u16,
}

impl StickyFooter {
    fn new() -> Self {
        Self {
            lines: vec![],
            drawn_cols: 0,
        }
    }

    /// Erase the footer by moving up one row per line and clearing.
    fn erase(&self, out: &mut io::Stdout) {
        if self.lines.is_empty() {
            return;
        }
        let _ = queue!(
            out,
            cursor::MoveUp(self.lines.len() as u16),
            terminal::Clear(ClearType::FromCursorDown),
        );
    }

    /// Erase the footer after a resize. Each original line was drawn at
    /// `drawn_cols` width. At the new (narrower) terminal width each line
    /// wraps to at most `ceil(drawn_cols / cols)` visual rows. We use that
    /// upper bound — overshooting is harmless because the cursor is at the
    /// very bottom of output and MoveUp cannot scroll past the first row of
    /// the viewport (the log lines above are untouched).
    fn erase_after_resize(&self, out: &mut io::Stdout) {
        if self.lines.is_empty() {
            return;
        }
        let (cols, _) = terminal::size().unwrap_or((80, 24));
        let rows_per_line = if self.drawn_cols > 0 && cols > 0 && cols < self.drawn_cols {
            self.drawn_cols.div_ceil(cols)
        } else {
            1
        };
        let total = self.lines.len() as u16 * rows_per_line;
        let _ = queue!(
            out,
            cursor::MoveUp(total),
            terminal::Clear(ClearType::FromCursorDown),
        );
    }

    fn draw(&mut self, out: &mut io::Stdout) {
        for line in &self.lines {
            let _ = write!(out, "{}\r\n", line);
        }
        // Re-hide cursor in case child process output leaked a show-cursor sequence.
        let _ = queue!(out, cursor::Hide);
        let _ = out.flush();
        self.drawn_cols = terminal::size().unwrap_or((80, 24)).0;
    }

    pub fn println(&mut self, msg: &str) {
        let mut out = io::stdout();
        self.erase(&mut out);
        let _ = write!(out, "{}", raw_terminal_block(msg));
        self.draw(&mut out);
    }

    pub fn set(&mut self, new_lines: Vec<String>) {
        let mut out = io::stdout();
        self.erase(&mut out);
        self.lines = new_lines;
        self.draw(&mut out);
    }

    /// Like `set`, but uses the resize-safe erase strategy.
    pub fn set_after_resize(&mut self, new_lines: Vec<String>) {
        let mut out = io::stdout();
        self.erase_after_resize(&mut out);
        self.lines = new_lines;
        self.draw(&mut out);
    }
}

// ── Shutdown signal ──────────────────────────────────────────────────────────

/// Resolves when a process-terminating signal (SIGTERM, SIGHUP) is received.
/// Used inside the output loop so that signals cause a clean exit with footer
/// cleanup instead of an abrupt process termination.
#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).ok();
    let mut hup = signal(SignalKind::hangup()).ok();
    tokio::select! {
        _ = async { if let Some(s) = &mut term { s.recv().await } else { None } } => {}
        _ = async { if let Some(s) = &mut hup { s.recv().await } else { None } } => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    std::future::pending::<()>().await
}

// ── Terminal guard ────────────────────────────────────────────────────────────

struct TerminalGuard;

impl TerminalGuard {
    fn enter(app_name: &str) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        let _ = execute!(stdout, cursor::Hide, SetTitle(format!("tako | {app_name}")),);
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, cursor::Show, SetTitle("tako"));
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn rawln(s: &str) {
    let mut out = io::stdout();
    let _ = write!(out, "{}\r\n", s);
    let _ = out.flush();
}

fn raw_terminal_block(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len() + 8);
    for line in msg.split('\n') {
        out.push('\r');
        out.push_str(line);
        out.push_str("\r\n");
    }
    out
}

fn spawn_key_reader(tx: mpsc::Sender<Event>) {
    std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            if tx.blocking_send(event).is_err() {
                break;
            }
        }
    });
}

// ── Footer state ──────────────────────────────────────────────────────────────

struct FooterState {
    repo_slug: String,
    repo_branch: String,
    repo_path: String,
    worktree_name: Option<String>,
    status: String,
    lan: ShareRowState,
    tunnel: ShareRowState,
    cpu: Option<f32>,
    mem_bytes: Option<u64>,
}

impl FooterState {
    fn new(
        repo_slug: String,
        repo_branch: String,
        repo_path: String,
        worktree_name: Option<String>,
    ) -> Self {
        Self {
            repo_slug,
            repo_branch,
            repo_path,
            worktree_name,
            status: "starting".to_string(),
            lan: ShareRowState::Inactive,
            tunnel: ShareRowState::Inactive,
            cpu: None,
            mem_bytes: None,
        }
    }

    fn build_lines(
        &self,
        app_name: &str,
        adapter_name: &str,
        hosts: &[String],
        port: u16,
    ) -> Vec<String> {
        let mut lines = format_panel(
            app_name,
            &self.status,
            adapter_name,
            &self.repo_slug,
            &self.repo_branch,
            &self.repo_path,
            self.worktree_name.as_deref(),
            hosts,
            port,
            ShareRows {
                lan: self.lan.clone(),
                tunnel: self.tunnel.clone(),
            },
            self.cpu,
            self.mem_bytes,
        )
        .lines()
        .map(|l| l.to_string())
        .collect::<Vec<_>>();
        lines.push(format_keymap());
        // Blank line above the panel separates it from the log stream.
        lines.insert(0, String::new());
        lines
    }

    fn refresh(
        &self,
        footer: &mut StickyFooter,
        app_name: &str,
        adapter_name: &str,
        hosts: &[String],
        port: u16,
    ) {
        footer.set(self.build_lines(app_name, adapter_name, hosts, port));
    }
}

fn lan_active_state(hosts: &[String]) -> ShareRowState {
    hosts
        .iter()
        .filter(|host| !host.starts_with("*."))
        .find_map(|host| to_local_route(host))
        .map(|route| ShareRowState::Active(format!("https://{route}")))
        .unwrap_or(ShareRowState::Failed)
}

// ── Loop exit tag (avoids moving channels inside select!) ─────────────────────

enum LoopExit {
    Terminate,
    Disconnect,
    Message(String),
}

// ── Main entry point ──────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub async fn run_dev_output(
    app_name: String,
    adapter_name: String,
    hosts: Vec<String>,
    port: u16,
    initial_lan_enabled: bool,
    initial_tunnel_url: Option<String>,
    mut log_rx: mpsc::Receiver<ScopedLog>,
    mut event_rx: mpsc::Receiver<DevEvent>,
    control_tx: mpsc::Sender<ControlCmd>,
) -> Result<DevOutputExit, Box<dyn std::error::Error>> {
    let _guard = TerminalGuard::enter(&app_name)?;
    super::output_render::set_app_runtime(adapter_name.clone());

    rawln("");
    for line in format_header().lines() {
        rawln(line);
    }
    rawln("");

    let (repo_slug, repo_branch, repo_path, worktree_name) = std::env::current_dir()
        .map(|cwd| git_info(&cwd))
        .unwrap_or_default();

    let mut footer = StickyFooter::new();
    let mut fs = FooterState::new(repo_slug, repo_branch, repo_path, worktree_name);
    if initial_lan_enabled {
        fs.lan = lan_active_state(&hosts);
    }
    if let Some(url) = initial_tunnel_url {
        fs.tunnel = ShareRowState::Active(url);
    }
    fs.refresh(&mut footer, &app_name, &adapter_name, &hosts, port);

    let (key_tx, mut key_rx) = mpsc::channel::<Event>(64);
    spawn_key_reader(key_tx);

    let mut sys = System::new();
    // 1-second ticker drives metrics refresh (every 2nd tick).
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    let mut tick_count = 0u64;
    let mut app_pid: Option<Pid> = None;

    // Debounce resize: each Resize event pushes the deadline forward.
    // When the deadline fires (no resize for 100ms), we do a full redraw.
    let mut resize_deadline: Option<tokio::time::Instant> = None;
    const RESIZE_DEBOUNCE: Duration = Duration::from_millis(100);

    // Catch SIGTERM / SIGHUP so the footer is cleaned up on signal-based exit.
    let sig = shutdown_signal();
    tokio::pin!(sig);

    // We break with a LoopExit tag to avoid moving log_rx/event_rx inside
    // the select! arms (they're borrowed by recv() arms).
    let loop_exit = loop {
        // Build the debounce future. `sleep_until` is only polled when a
        // resize is pending; otherwise we use a future that never resolves.
        let resize_sleep = match resize_deadline {
            Some(dl) => tokio::time::sleep_until(dl),
            None => tokio::time::sleep(Duration::from_secs(86400)),
        };
        let has_resize = resize_deadline.is_some();
        tokio::pin!(resize_sleep);

        tokio::select! {
            _ = &mut sig => {
                break LoopExit::Terminate;
            }
            // Debounced resize: fires 100ms after the last Resize event.
            _ = &mut resize_sleep, if has_resize => {
                resize_deadline = None;
                footer.set_after_resize(
                    fs.build_lines(&app_name, &adapter_name, &hosts, port),
                );
            }
            _ = ticker.tick() => {
                tick_count += 1;

                // Refresh metrics every 2 seconds; only redraw if values changed.
                if tick_count.is_multiple_of(METRICS_REFRESH_SECS)
                    && let Some(pid) = app_pid {
                        sys.refresh_processes(ProcessesToUpdate::All, false);
                        if let Some((cpu, mem)) = process_tree_metrics(&sys, pid) {
                            let changed = fs.cpu != Some(cpu) || fs.mem_bytes != Some(mem);
                            fs.cpu = Some(cpu);
                            fs.mem_bytes = Some(mem);
                            if changed {
                                fs.refresh(&mut footer, &app_name, &adapter_name, &hosts, port);
                            }
                        }
                    }
            }
            Some(log) = log_rx.recv() => {
                footer.println(&format_log(&log));
            }
            event = event_rx.recv() => {
                let Some(event) = event else {
                    // All event senders dropped — client ended.
                    break LoopExit::Terminate;
                };
                match event {
                    DevEvent::AppStarted => {
                        fs.status = "starting".to_string();
                        fs.refresh(&mut footer, &app_name, &adapter_name, &hosts, port);
                    }
                    DevEvent::AppReady => {
                        fs.status = "running".to_string();
                        if let Some(pid) = app_pid {
                            sys.refresh_processes(ProcessesToUpdate::All, false);
                            if let Some((cpu, mem)) = process_tree_metrics(&sys, pid) {
                                fs.cpu = Some(cpu);
                                fs.mem_bytes = Some(mem);
                            }
                        }
                        fs.refresh(&mut footer, &app_name, &adapter_name, &hosts, port);
                        footer.println(&format_log(&ScopedLog::info(
                            "tako",
                            "App started".to_string(),
                        )));
                    }
                    DevEvent::AppLaunching => {
                        fs.status = "launching…".to_string();
                        fs.refresh(&mut footer, &app_name, &adapter_name, &hosts, port);
                    }
                    DevEvent::AppStopped => {
                        app_pid = None;
                        fs.cpu = None;
                        fs.mem_bytes = None;
                        fs.status = "stopped".to_string();
                        fs.refresh(&mut footer, &app_name, &adapter_name, &hosts, port);
                    }
                    DevEvent::AppPid(pid) => {
                        app_pid = Some(Pid::from(pid as usize));
                        fs.refresh(&mut footer, &app_name, &adapter_name, &hosts, port);
                    }
                    DevEvent::AppProcessExited(_) => {
                        app_pid = None;
                        fs.cpu = None;
                        fs.mem_bytes = None;
                        fs.status = "exited".to_string();
                        fs.refresh(&mut footer, &app_name, &adapter_name, &hosts, port);
                    }
                    DevEvent::AppError(ref e) => {
                        fs.status = "error".to_string();
                        fs.refresh(&mut footer, &app_name, &adapter_name, &hosts, port);
                        footer.println(&format!("\x1b[38;2;232;163;160merror:{RESET} {e}"));
                    }
                    DevEvent::ClientConnected { is_self, client_id } => {
                        if is_self {
                            footer.println(&format!("{DIM}──── connected ────{RESET}"));
                        } else {
                            footer.println(&format_log(&ScopedLog::at(
                                LogLevel::Debug,
                                "tako",
                                format!("Client {} connected", client_id),
                            )));
                        }
                    }
                    DevEvent::ClientDisconnected { client_id } => {
                        footer.println(&format_log(&ScopedLog::at(
                            LogLevel::Debug,
                            "tako",
                            format!("Client {} disconnected", client_id),
                        )));
                    }
                    DevEvent::LanModeChanged {
                        enabled,
                        lan_ip: _,
                        ca_url,
                    } => {
                        fs.lan = if enabled {
                            lan_active_state(&hosts)
                        } else {
                            ShareRowState::Inactive
                        };
                        fs.refresh(&mut footer, &app_name, &adapter_name, &hosts, port);
                        if enabled
                            && let Some(ref url) = ca_url {
                                for line in format_lan_block(&hosts, url) {
                                    footer.println(&line);
                                }
                            }
                    }
                    DevEvent::LanStarting => {}
                    DevEvent::LanFailed => {
                        fs.lan = ShareRowState::Failed;
                        fs.refresh(&mut footer, &app_name, &adapter_name, &hosts, port);
                    }
                    DevEvent::TunnelModeChanged {
                        enabled,
                        url,
                        close_reason,
                        ..
                    } => {
                        fs.tunnel = if enabled {
                            url.clone()
                                .map(ShareRowState::Active)
                                .unwrap_or(ShareRowState::Failed)
                        } else {
                            ShareRowState::Inactive
                        };
                        fs.refresh(&mut footer, &app_name, &adapter_name, &hosts, port);
                        if enabled
                            && let Some(url) = url {
                                for line in format_tunnel_block(&url) {
                                    footer.println(&line);
                                }
                        } else if !enabled {
                            footer.println(&format_log(&tunnel_close_log(close_reason)));
                        }
                    }
                    DevEvent::TunnelConnectionChanged { connected, url } => {
                        fs.tunnel = if connected {
                            ShareRowState::Active(url)
                        } else {
                            ShareRowState::Reconnecting(url)
                        };
                        fs.refresh(&mut footer, &app_name, &adapter_name, &hosts, port);
                        footer.println(&format_log(&tunnel_connection_log(connected)));
                    }
                    DevEvent::TunnelStarting => {
                        fs.tunnel = ShareRowState::Starting;
                        fs.refresh(&mut footer, &app_name, &adapter_name, &hosts, port);
                    }
                    DevEvent::TunnelFailed => {
                        fs.tunnel = ShareRowState::Failed;
                        fs.refresh(&mut footer, &app_name, &adapter_name, &hosts, port);
                    }
                    DevEvent::ExitWithMessage(msg) => {
                        break LoopExit::Message(msg);
                    }
                }
            }
            Some(event) = key_rx.recv() => {
                match event {
                    Event::Key(key) => match key.code {
                        KeyCode::Char('c')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            let _ = control_tx.send(ControlCmd::Terminate).await;
                            break LoopExit::Terminate;
                        }
                        KeyCode::Char('q') | KeyCode::Char('Q') => {
                            let _ = control_tx.send(ControlCmd::Terminate).await;
                            break LoopExit::Terminate;
                        }
                        KeyCode::Char('r') | KeyCode::Char('R') => {
                            let _ = control_tx.send(ControlCmd::Restart).await;
                        }
                        KeyCode::Char('b') | KeyCode::Char('B') => {
                            break LoopExit::Disconnect;
                        }
                        KeyCode::Char('l') | KeyCode::Char('L') => {
                            let _ = control_tx.send(ControlCmd::ToggleLan).await;
                        }
                        KeyCode::Char('t') | KeyCode::Char('T') => {
                            let _ = control_tx.send(ControlCmd::ToggleTunnel).await;
                        }
                        _ => {}
                    },
                    Event::Resize(_, _) => {
                        // Immediately erase the (possibly wrapped) footer so
                        // remnants don't linger, then debounce the redraw.
                        {
                            let mut out = io::stdout();
                            footer.erase_after_resize(&mut out);
                            let _ = out.flush();
                        }
                        footer.lines.clear();
                        resize_deadline = Some(tokio::time::Instant::now() + RESIZE_DEBOUNCE);
                    }
                    _ => {}
                }
            }
        }
    };

    // Erase the footer before exiting so the terminal is clean.
    {
        let mut out = io::stdout();
        footer.erase(&mut out);
        let _ = out.flush();
    }

    // Restore terminal state *before* printing the exit line.
    // Dropping the guard here (rather than letting it drop at end-of-scope)
    // avoids the extra blank line that can appear when raw mode is disabled
    // after a \r\n has already moved the cursor down.
    drop(_guard);

    // Build the exit value (now that log_rx/event_rx are no longer borrowed).
    let exit = match loop_exit {
        LoopExit::Terminate => {
            println!("\n{DIM}{app_name} stopped{RESET}");
            DevOutputExit::Terminate
        }
        LoopExit::Disconnect => {
            println!();
            println!(
                "{DIM}{app_name} is running in the background — run `tako dev` to reconnect{RESET}"
            );
            DevOutputExit::Disconnect { log_rx, event_rx }
        }
        LoopExit::Message(msg) => {
            println!("\n{DIM}{app_name} {msg}{RESET}");
            DevOutputExit::Terminate
        }
    };

    Ok(exit)
}

#[cfg(test)]
mod tests;
