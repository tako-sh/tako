use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::output;
use crate::ui::{TaskIcon, TaskItemState, TaskState, TaskTreeSession, TreeNode, TreeTextTone};

pub(super) fn should_use_upgrade_task_tree() -> bool {
    output::is_pretty() && output::is_interactive()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    VersionCheck,
    Upgrade,
}

impl Step {
    fn suffix(self) -> &'static str {
        match self {
            Step::VersionCheck => "version-check",
            Step::Upgrade => "upgrade",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Step::VersionCheck => "Getting current version",
            Step::Upgrade => "Upgrading",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct UpgradeTaskTreeState {
    pub(super) servers: Vec<TaskItemState>,
    pub(super) error_summary: Option<String>,
}

#[derive(Clone)]
pub(super) struct UpgradeTaskTreeController {
    state: Arc<Mutex<UpgradeTaskTreeState>>,
    session: Option<TaskTreeSession>,
}

impl UpgradeTaskTreeController {
    pub(super) fn new(server_names: &[String]) -> Self {
        let servers = server_names
            .iter()
            .map(|name| {
                TaskItemState::pending(server_task_id(name), name.clone())
                    .with_icon(TaskIcon::None)
                    .with_children(vec![
                        boxed_task(
                            step_task_id(name, Step::VersionCheck),
                            Step::VersionCheck.label(),
                        ),
                        boxed_task(step_task_id(name, Step::Upgrade), Step::Upgrade.label()),
                    ])
            })
            .collect();
        let state = UpgradeTaskTreeState {
            servers,
            error_summary: None,
        };
        let tree = build_tree(&state);
        let session = should_use_upgrade_task_tree().then(|| TaskTreeSession::new(tree));
        Self {
            state: Arc::new(Mutex::new(state)),
            session,
        }
    }

    pub(super) fn mark_server_running(&self, name: &str) {
        let mut state = self.state.lock().unwrap();
        if let Some(server) = find_server_mut(&mut state, name) {
            set_running(server);
        }
        self.refresh_locked(&state);
    }

    pub(super) fn mark_step_running(&self, name: &str, step: Step) {
        let mut state = self.state.lock().unwrap();
        if let Some(server) = find_server_mut(&mut state, name)
            && let Some(child) = find_step_mut(server, step)
        {
            set_running(child);
        }
        self.refresh_locked(&state);
    }

    pub(super) fn rename_step(&self, name: &str, step: Step, new_label: impl Into<String>) {
        let mut state = self.state.lock().unwrap();
        if let Some(server) = find_server_mut(&mut state, name)
            && let Some(child) = find_step_mut(server, step)
        {
            child.label = new_label.into();
        }
        self.refresh_locked(&state);
    }

    pub(super) fn succeed_step(&self, name: &str, step: Step, detail: Option<String>) {
        let mut state = self.state.lock().unwrap();
        if let Some(server) = find_server_mut(&mut state, name)
            && let Some(child) = find_step_mut(server, step)
        {
            set_succeeded(child, detail);
        }
        self.refresh_locked(&state);
    }

    pub(super) fn fail_step(&self, name: &str, step: Step, detail: impl Into<String>) {
        let mut state = self.state.lock().unwrap();
        if let Some(server) = find_server_mut(&mut state, name) {
            if let Some(child) = find_step_mut(server, step) {
                set_failed(child, Some(detail.into()));
            }
            cancel_pending_children(server);
        }
        self.refresh_locked(&state);
    }

    pub(super) fn succeed_server(&self, name: &str, detail: Option<String>) {
        let mut state = self.state.lock().unwrap();
        if let Some(server) = find_server_mut(&mut state, name) {
            set_succeeded(server, detail);
        }
        self.refresh_locked(&state);
    }

    pub(super) fn fail_server(&self, name: &str) {
        let mut state = self.state.lock().unwrap();
        if let Some(server) = find_server_mut(&mut state, name) {
            set_failed(server, None);
        }
        self.refresh_locked(&state);
    }

    pub(super) fn set_error_summary(&self, summary: String) {
        let mut state = self.state.lock().unwrap();
        state.error_summary = Some(summary);
        self.refresh_locked(&state);
    }

    pub(super) fn finalize(&self) {
        if let Some(session) = &self.session {
            session.finalize();
        }
    }

    fn refresh_locked(&self, state: &UpgradeTaskTreeState) {
        if let Some(session) = &self.session {
            session.set_tree(build_tree(state));
        }
    }

    #[cfg(test)]
    pub(super) fn snapshot(&self) -> UpgradeTaskTreeState {
        self.state.lock().unwrap().clone()
    }
}

fn server_task_id(name: &str) -> String {
    format!("server:{name}")
}

fn step_task_id(name: &str, step: Step) -> String {
    format!("server:{name}:{}", step.suffix())
}

fn find_server_mut<'a>(
    state: &'a mut UpgradeTaskTreeState,
    name: &str,
) -> Option<&'a mut TaskItemState> {
    let id = server_task_id(name);
    state.servers.iter_mut().find(|t| t.id == id)
}

fn find_step_mut(server: &mut TaskItemState, step: Step) -> Option<&mut TaskItemState> {
    let suffix = format!(":{}", step.suffix());
    server.children.iter_mut().find(|c| c.id.ends_with(&suffix))
}

fn set_running(task: &mut TaskItemState) {
    if matches!(task.state, TaskState::Pending) {
        task.state = TaskState::Running {
            started_at: Instant::now(),
        };
        task.detail = None;
    }
}

fn set_succeeded(task: &mut TaskItemState, detail: Option<String>) {
    let elapsed = match task.state {
        TaskState::Running { started_at } => Some(started_at.elapsed()),
        _ => None,
    };
    task.state = TaskState::Succeeded { elapsed };
    task.detail = detail;
}

fn set_failed(task: &mut TaskItemState, detail: Option<String>) {
    let elapsed = match task.state {
        TaskState::Running { started_at } => Some(started_at.elapsed()),
        _ => None,
    };
    task.state = TaskState::Failed { elapsed };
    task.detail = detail;
}

fn cancel_pending_children(parent: &mut TaskItemState) {
    for child in &mut parent.children {
        if matches!(child.state, TaskState::Pending) {
            child.state = TaskState::Cancelled { elapsed: None };
        }
    }
}

fn boxed_task(id: impl Into<String>, label: impl Into<String>) -> TaskItemState {
    TaskItemState::pending(id, label).with_icon(TaskIcon::Box)
}

pub(super) fn build_tree(state: &UpgradeTaskTreeState) -> Vec<TreeNode> {
    let mut tree = Vec::new();
    for (index, server) in state.servers.iter().enumerate() {
        tree.push(TreeNode::Task(server.clone()));
        if index + 1 < state.servers.len() {
            tree.push(TreeNode::Spacer);
        }
    }

    if let Some(summary) = &state.error_summary {
        if !tree.is_empty() && !matches!(tree.last(), Some(TreeNode::Spacer)) {
            tree.push(TreeNode::Spacer);
        }
        tree.push(TreeNode::Text {
            text: summary.clone(),
            tone: TreeTextTone::Error,
        });
    }

    tree
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller_for(names: &[&str]) -> UpgradeTaskTreeController {
        let owned: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        UpgradeTaskTreeController::new(&owned)
    }

    fn task_node(tree: &[TreeNode], idx: usize) -> &TaskItemState {
        match &tree[idx] {
            TreeNode::Task(task) => task,
            other => panic!("expected Task at index {idx}, got {other:?}"),
        }
    }

    #[test]
    fn new_creates_per_server_task_with_two_pending_subtasks() {
        let controller = controller_for(&["prod-a", "prod-b"]);
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.servers.len(), 2);
        for server in &snapshot.servers {
            assert!(matches!(server.state, TaskState::Pending));
            assert_eq!(server.children.len(), 2);
            assert_eq!(server.children[0].label, "Getting current version");
            assert_eq!(server.children[1].label, "Upgrading");
        }
    }

    #[test]
    fn tree_inserts_spacer_between_servers() {
        let controller = controller_for(&["prod-a", "prod-b"]);
        let tree = build_tree(&controller.snapshot());
        assert!(matches!(tree[0], TreeNode::Task(_)));
        assert!(matches!(tree[1], TreeNode::Spacer));
        assert!(matches!(tree[2], TreeNode::Task(_)));
    }

    #[test]
    fn running_upgrade_uses_group_and_box_icons() {
        let controller = controller_for(&["prod-a"]);
        controller.mark_server_running("prod-a");
        controller.mark_step_running("prod-a", Step::VersionCheck);

        let lines = crate::ui::render_plain_lines(&build_tree(&controller.snapshot()));

        assert!(lines.iter().any(|line| line.starts_with("prod-a…")));
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("  ◧ Getting current version…"))
        );
        assert!(lines.iter().any(|line| line == "  □ Upgrading…"));
    }

    #[test]
    fn already_on_latest_renames_upgrade_step() {
        let controller = controller_for(&["prod-a"]);
        controller.mark_server_running("prod-a");
        controller.mark_step_running("prod-a", Step::VersionCheck);
        controller.rename_step("prod-a", Step::VersionCheck, "Current version: 0.0.2");
        controller.succeed_step("prod-a", Step::VersionCheck, None);
        controller.rename_step("prod-a", Step::Upgrade, "Already on latest");
        controller.succeed_step("prod-a", Step::Upgrade, None);
        controller.succeed_server("prod-a", None);

        let tree = build_tree(&controller.snapshot());
        let parent = task_node(&tree, 0);
        assert!(matches!(parent.state, TaskState::Succeeded { .. }));
        assert_eq!(parent.detail, None);
        assert_eq!(parent.children[0].label, "Current version: 0.0.2");
        assert_eq!(parent.children[1].label, "Already on latest");
        assert!(matches!(
            parent.children[1].state,
            TaskState::Succeeded { .. }
        ));
    }

    #[test]
    fn version_check_failure_cancels_upgrade_step() {
        let controller = controller_for(&["prod-a"]);
        controller.mark_server_running("prod-a");
        controller.mark_step_running("prod-a", Step::VersionCheck);
        controller.fail_step("prod-a", Step::VersionCheck, "ssh error");
        controller.fail_server("prod-a");
        controller.set_error_summary("0/1 servers upgraded".to_string());

        let tree = build_tree(&controller.snapshot());
        let parent = task_node(&tree, 0);
        assert!(matches!(parent.state, TaskState::Failed { .. }));
        assert!(matches!(parent.children[0].state, TaskState::Failed { .. }));
        assert!(matches!(
            parent.children[1].state,
            TaskState::Cancelled { .. }
        ));
        match &tree[2] {
            TreeNode::Text { text, .. } => assert_eq!(text, "0/1 servers upgraded"),
            _ => panic!("expected summary text"),
        }
    }

    #[test]
    fn happy_path_renames_upgrade_step_to_upgraded() {
        let controller = controller_for(&["prod-a"]);
        controller.mark_server_running("prod-a");
        controller.mark_step_running("prod-a", Step::VersionCheck);
        controller.rename_step("prod-a", Step::VersionCheck, "Current version: 0.0.1");
        controller.succeed_step("prod-a", Step::VersionCheck, None);
        controller.mark_step_running("prod-a", Step::Upgrade);
        controller.rename_step("prod-a", Step::Upgrade, "Upgraded");
        controller.succeed_step("prod-a", Step::Upgrade, None);
        controller.succeed_server("prod-a", None);

        let tree = build_tree(&controller.snapshot());
        let parent = task_node(&tree, 0);
        assert!(matches!(parent.state, TaskState::Succeeded { .. }));
        assert_eq!(parent.detail, None);
        assert_eq!(parent.children[1].label, "Upgraded");
        assert!(matches!(
            parent.children[0].state,
            TaskState::Succeeded { .. }
        ));
        assert!(matches!(
            parent.children[1].state,
            TaskState::Succeeded { .. }
        ));
    }
}
