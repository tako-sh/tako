use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::output;
use crate::ui::{TaskIcon, TaskItemState, TaskState, TaskTreeSession, TreeNode};

const UPGRADE_TASK_ID: &str = "cli-upgrade";

#[derive(Debug, Clone)]
pub(super) struct LocalUpgradeTaskState {
    pub(super) task: TaskItemState,
}

#[derive(Clone)]
pub(super) struct LocalUpgradeTask {
    state: Arc<Mutex<LocalUpgradeTaskState>>,
    session: Option<TaskTreeSession>,
}

impl LocalUpgradeTask {
    pub(super) fn start(label: &str) -> Self {
        let mut task = TaskItemState::pending(UPGRADE_TASK_ID, label).with_icon(TaskIcon::State);
        set_running(&mut task);
        let state = LocalUpgradeTaskState { task };
        let tree = build_tree(&state);
        let session = should_use_local_upgrade_task_tree().then(|| TaskTreeSession::new(tree));

        Self {
            state: Arc::new(Mutex::new(state)),
            session,
        }
    }

    pub(super) fn is_rendered(&self) -> bool {
        self.session.is_some()
    }

    pub(super) fn succeed(&self, label: impl Into<String>) {
        let mut state = self.state.lock().unwrap();
        state.task.label = label.into();
        set_succeeded(&mut state.task);
        self.refresh_locked(&state);
        self.finalize();
    }

    pub(super) fn fail(&self, label: impl Into<String>, detail: impl Into<String>) {
        let mut state = self.state.lock().unwrap();
        state.task.label = label.into();
        set_failed(&mut state.task, Some(detail.into()));
        self.refresh_locked(&state);
        self.finalize();
    }

    fn finalize(&self) {
        if let Some(session) = &self.session {
            session.finalize();
        }
    }

    fn refresh_locked(&self, state: &LocalUpgradeTaskState) {
        if let Some(session) = &self.session {
            session.set_tree(build_tree(state));
        }
    }

    #[cfg(test)]
    pub(super) fn snapshot(&self) -> LocalUpgradeTaskState {
        self.state.lock().unwrap().clone()
    }
}

fn should_use_local_upgrade_task_tree() -> bool {
    output::is_pretty() && output::is_interactive()
}

fn set_running(task: &mut TaskItemState) {
    task.state = TaskState::Running {
        started_at: Instant::now(),
    };
    task.detail = None;
}

fn set_succeeded(task: &mut TaskItemState) {
    let elapsed = match task.state {
        TaskState::Running { started_at } => Some(started_at.elapsed()),
        _ => None,
    };
    task.state = TaskState::Succeeded { elapsed };
    task.detail = None;
}

fn set_failed(task: &mut TaskItemState, detail: Option<String>) {
    let elapsed = match task.state {
        TaskState::Running { started_at } => Some(started_at.elapsed()),
        _ => None,
    };
    task.state = TaskState::Failed { elapsed };
    task.detail = detail;
}

pub(super) fn build_tree(state: &LocalUpgradeTaskState) -> Vec<TreeNode> {
    vec![TreeNode::Spacer, TreeNode::AccentTask(state.task.clone())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_blank_line_then_running_upgrade_task() {
        let task = LocalUpgradeTask::start("Upgrading");
        let lines = crate::ui::render_plain_lines(&build_tree(&task.snapshot()));

        assert_eq!(lines.first().map(String::as_str), Some(""));
        assert_eq!(lines.get(1).map(String::as_str), Some("◧ Upgrading…"));
    }

    #[test]
    fn success_renames_upgrade_task() {
        let task = LocalUpgradeTask::start("Upgrading");
        task.succeed("Upgraded to 0.0.0-258982c");

        let lines = crate::ui::render_plain_lines(&build_tree(&task.snapshot()));

        assert_eq!(lines.first().map(String::as_str), Some(""));
        assert!(
            lines
                .get(1)
                .is_some_and(|line| line.starts_with("✔ Upgraded to 0.0.0-258982c"))
        );
    }

    #[test]
    fn failure_keeps_detail_under_failed_task() {
        let task = LocalUpgradeTask::start("Upgrading");
        task.fail("Upgrade failed", "download failed");

        let lines = crate::ui::render_plain_lines(&build_tree(&task.snapshot()));

        assert_eq!(lines.first().map(String::as_str), Some(""));
        assert!(
            lines
                .get(1)
                .is_some_and(|line| line.starts_with("✘ Upgrade failed"))
        );
        assert_eq!(lines.get(2).map(String::as_str), Some("  download failed"));
    }
}
