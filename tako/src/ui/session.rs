use std::io;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::Duration;

use ratatui::backend::CrosstermBackend;
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::output;

use super::render::{render_tree_to_lines, rendered_height, tree_node_has_running};
use super::{TaskItemState, TaskState, TreeNode, TreeTextTone};

const LIVE_RENDER_INTERVAL: Duration = Duration::from_millis(80);

/// Row below the active inline viewport, updated on each draw.
static VIEWPORT_BOTTOM_ROW: AtomicU16 = AtomicU16::new(0);
static ACTIVE_SESSION: Mutex<Option<Weak<SessionShared>>> = Mutex::new(None);

type RatatuiTerminal = Terminal<CrosstermBackend<io::Stderr>>;

#[derive(Clone)]
pub struct TaskTreeSession {
    shared: Arc<SessionShared>,
}

struct SessionShared {
    enabled: bool,
    stop: AtomicBool,
    finalized: AtomicBool,
    state: Mutex<SessionState>,
    tick_thread: Mutex<Option<thread::JoinHandle<()>>>,
    terminal: Mutex<Option<RatatuiTerminal>>,
}

struct SessionState {
    tree: Vec<TreeNode>,
    paused: bool,
    frame_index: usize,
}

/// Minimal cleanup for Ctrl-C: move cursor below the viewport.
pub fn cleanup_on_interrupt() {
    let row = VIEWPORT_BOTTOM_ROW.load(Ordering::Relaxed);
    if row > 0 {
        let _ = crossterm::execute!(io::stderr(), crossterm::cursor::MoveTo(0, row));
    }
}

impl TaskTreeSession {
    pub fn new(tree: Vec<TreeNode>) -> Self {
        let enabled = output::is_pretty() && output::is_interactive();

        let terminal = if enabled {
            create_inline_terminal(&tree).ok()
        } else {
            None
        };

        let shared = Arc::new(SessionShared {
            enabled,
            stop: AtomicBool::new(false),
            finalized: AtomicBool::new(false),
            state: Mutex::new(SessionState {
                tree,
                paused: false,
                frame_index: 0,
            }),
            tick_thread: Mutex::new(None),
            terminal: Mutex::new(terminal),
        });

        let session = Self {
            shared: shared.clone(),
        };

        if enabled {
            *ACTIVE_SESSION.lock().unwrap() = Some(Arc::downgrade(&shared));
            session.draw_now();
            let thread_shared = shared.clone();
            let handle = thread::spawn(move || {
                while !thread_shared.stop.load(Ordering::Relaxed) {
                    thread::sleep(LIVE_RENDER_INTERVAL);
                    if thread_shared.stop.load(Ordering::Relaxed) {
                        break;
                    }

                    let should_draw = {
                        let mut state = thread_shared.state.lock().unwrap();
                        let has_running = state.tree.iter().any(tree_node_has_running);
                        if state.paused || !has_running {
                            false
                        } else {
                            state.frame_index =
                                (state.frame_index + 1) % output::SPINNER_TICKS.len();
                            true
                        }
                    };

                    if should_draw {
                        draw_shared(&thread_shared);
                    }
                }
            });
            *shared.tick_thread.lock().unwrap() = Some(handle);
        }

        session
    }

    pub fn set_tree(&self, tree: Vec<TreeNode>) {
        {
            let mut state = self.shared.state.lock().unwrap();
            state.tree = tree;
        }
        if self.shared.enabled {
            self.draw_now();
        }
    }

    /// Explicitly finalize this session: stop the tick thread and render the
    /// final tree output. Subsequent drops become no-ops. Call this before the
    /// function returns so the output appears *before* the shell prompt.
    pub fn finalize(&self) {
        if self.shared.finalized.swap(true, Ordering::Relaxed) {
            return;
        }
        finalize_shared_session(&self.shared);
    }

    fn draw_now(&self) {
        if !self.shared.enabled {
            return;
        }
        draw_shared(&self.shared);
    }
}

impl Drop for TaskTreeSession {
    fn drop(&mut self) {
        if Arc::strong_count(&self.shared) != 1 {
            return;
        }
        if self.shared.finalized.load(Ordering::Relaxed) {
            return;
        }
        finalize_shared_session(&self.shared);
    }
}

pub fn interrupt_with_message(message: &str) -> bool {
    let shared = ACTIVE_SESSION
        .lock()
        .unwrap()
        .as_ref()
        .and_then(Weak::upgrade);
    let Some(shared) = shared else {
        return false;
    };
    if !shared.enabled {
        return false;
    }

    {
        let mut state = shared.state.lock().unwrap();
        append_interrupt_message(&mut state.tree, message);
    }
    draw_shared(&shared);
    true
}

pub fn finalize_active_session() -> bool {
    let shared = ACTIVE_SESSION
        .lock()
        .unwrap()
        .as_ref()
        .and_then(Weak::upgrade);
    let Some(shared) = shared else {
        return false;
    };
    finalize_shared_session(&shared);
    true
}

fn create_inline_terminal(tree: &[TreeNode]) -> io::Result<RatatuiTerminal> {
    let width = crossterm::terminal::size()?.0;
    let height = rendered_height(&render_tree_to_lines(tree, 0), width);
    let height = height.clamp(4, 20);
    let backend = CrosstermBackend::new(io::stderr());
    Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
}

fn draw_shared(shared: &SessionShared) {
    let lines = {
        let state = shared.state.lock().unwrap();
        if state.paused {
            return;
        }
        render_tree_to_lines(&state.tree, state.frame_index)
    };

    if let Ok(mut term_guard) = shared.terminal.lock()
        && let Some(term) = term_guard.as_mut()
    {
        let width = term.size().map(|s| s.width).unwrap_or(80);
        let needed = rendered_height(&lines, width);
        let current = term.size().map(|s| s.height).unwrap_or(0);
        if needed > current {
            let _ = term.resize(ratatui::layout::Rect::new(
                0,
                0,
                term.size().map(|s| s.width).unwrap_or(80),
                needed.min(20),
            ));
        }
        let _ = term.draw(|frame| {
            let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
            frame.render_widget(paragraph, frame.area());
        });
        let area = term.get_frame().area();
        VIEWPORT_BOTTOM_ROW.store(area.y + area.height, Ordering::Relaxed);
    }
}

fn finalize_shared_session(shared: &Arc<SessionShared>) {
    {
        let mut active = ACTIVE_SESSION.lock().unwrap();
        let is_this_session = active
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|a| Arc::ptr_eq(&a, shared));
        if is_this_session {
            *active = None;
        }
    }

    shared.stop.store(true, Ordering::Relaxed);
    if let Some(handle) = shared.tick_thread.lock().unwrap().take() {
        let _ = handle.join();
    }

    if !shared.enabled {
        return;
    }

    if let Ok(mut term_guard) = shared.terminal.lock()
        && let Some(term) = term_guard.as_mut()
    {
        let lines = {
            let state = shared.state.lock().unwrap();
            render_tree_to_lines(&state.tree, state.frame_index)
        };
        let width = term.size().map(|s| s.width).unwrap_or(80);
        let height = rendered_height(&lines, width);
        let _ = term.insert_before(height, |buf| {
            Paragraph::new(lines.clone())
                .wrap(Wrap { trim: false })
                .render(buf.area, buf);
        });
    }
}

pub(super) fn append_interrupt_message(tree: &mut Vec<TreeNode>, message: &str) {
    let already_appended = matches!(
        tree.last(),
        Some(TreeNode::Text { text, tone: TreeTextTone::Error }) if text == message
    );
    if already_appended {
        return;
    }

    for node in tree.iter_mut() {
        match node {
            TreeNode::Task(task) | TreeNode::AccentTask(task) => {
                cancel_running_task(task);
            }
            _ => {}
        }
    }

    if !tree.is_empty() && !matches!(tree.last(), Some(TreeNode::Spacer)) {
        tree.push(TreeNode::Spacer);
    }
    tree.push(TreeNode::Text {
        text: message.to_string(),
        tone: TreeTextTone::Error,
    });
}

fn cancel_running_task(task: &mut TaskItemState) {
    for child in &mut task.children {
        cancel_running_task(child);
    }
    if let TaskState::Running { started_at } = task.state {
        task.state = TaskState::Cancelled {
            elapsed: Some(started_at.elapsed()),
        };
    }
}
