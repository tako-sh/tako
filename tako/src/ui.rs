use std::time::{Duration, Instant};

mod render;
mod session;

#[cfg(test)]
mod tests;

#[cfg(test)]
pub use render::render_plain_lines;
pub use session::{
    TaskTreeSession, cleanup_on_interrupt, finalize_active_session, interrupt_with_message,
};

const TASK_INDENT: &str = "  ";
/// Spaces between a row's label/detail and elapsed time in task-tree output.
const ELAPSED_GAP: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    Pending,
    Running { started_at: Instant },
    Succeeded { elapsed: Option<Duration> },
    Failed { elapsed: Option<Duration> },
    Skipped { elapsed: Option<Duration> },
    Cancelled { elapsed: Option<Duration> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskItemState {
    pub id: String,
    pub label: String,
    pub state: TaskState,
    pub icon: TaskIcon,
    pub detail: Option<String>,
    /// Progress fraction (0.0-1.0) rendered as a native ratatui block bar.
    pub progress: Option<f64>,
    pub children: Vec<TaskItemState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskIcon {
    None,
    State,
    Box,
}

impl TaskItemState {
    pub fn pending(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            state: TaskState::Pending,
            icon: TaskIcon::State,
            detail: None,
            progress: None,
            children: Vec::new(),
        }
    }

    pub fn with_children(mut self, children: Vec<TaskItemState>) -> Self {
        self.children = children;
        self
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn with_icon(mut self, icon: TaskIcon) -> Self {
        self.icon = icon;
        self
    }

    pub fn append_child(&mut self, child: TaskItemState) {
        self.children.push(child);
    }

    pub fn find(&self, id: &str) -> Option<&TaskItemState> {
        if self.id == id {
            return Some(self);
        }
        self.children.iter().find_map(|child| child.find(id))
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut TaskItemState> {
        if self.id == id {
            return Some(self);
        }
        self.children
            .iter_mut()
            .find_map(|child| child.find_mut(id))
    }
}

#[derive(Debug, Clone)]
pub enum TreeTextTone {
    Error,
}

#[derive(Debug, Clone)]
pub enum TreeNode {
    /// A task item (leaf or group with children).
    Task(TaskItemState),
    /// An accent task item rendered as a top-level reporter (e.g., "Built 3.4s").
    AccentTask(TaskItemState),
    /// A non-task text row.
    Text { text: String, tone: TreeTextTone },
    /// Label (accent) + value (normal) on one line, with a space between.
    LabeledText { label: String, value: String },
    /// Blank spacer line.
    Spacer,
}
