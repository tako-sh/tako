use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::config::ServerTarget;
use crate::output;
use crate::ui::{TaskItemState, TaskState, TaskTreeSession, TreeNode, TreeTextTone};

use super::format::{
    SummaryLine, format_build_plan_target_label, format_deploy_summary_lines_with_https_port,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ArtifactBuildGroup {
    pub(super) build_target_label: String,
    pub(super) cache_target_label: String,
    pub(super) target_labels: Vec<String>,
    pub(super) display_target_label: Option<String>,
}

pub(super) const UNIFIED_JS_CACHE_TARGET_LABEL: &str = "shared-local-js";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct LocalArtifactCacheCleanupSummary {
    pub(super) removed_target_artifacts: usize,
    pub(super) removed_target_metadata: usize,
}

impl LocalArtifactCacheCleanupSummary {
    pub(super) fn total_removed(self) -> usize {
        self.removed_target_artifacts + self.removed_target_metadata
    }
}

#[derive(Debug, Clone)]
pub(super) struct DeployTaskTreeState {
    pub(super) builds: Vec<TaskItemState>,
    pub(super) deploys: Vec<TaskItemState>,
    pub(super) success_lines: Vec<SummaryLine>,
    pub(super) summary_line: Option<(String, TreeTextTone)>,
}

#[derive(Clone)]
pub(super) struct DeployTaskTreeController {
    state: Arc<Mutex<DeployTaskTreeState>>,
    session: Option<TaskTreeSession>,
}

#[derive(Clone, Copy)]
pub(super) enum DeployCompletionKind {
    Succeeded,
    Skipped,
    Failed,
    Cancelled,
}

impl DeployTaskTreeController {
    pub(super) fn new(server_names: &[String], build_groups: &[ArtifactBuildGroup]) -> Self {
        let state = DeployTaskTreeState {
            builds: build_groups
                .iter()
                .map(|group| {
                    let label = format_build_plan_target_label(group);
                    TaskItemState::pending(build_target_task_id(&label), label.clone())
                        .with_children(vec![
                            TaskItemState::pending(
                                build_task_step_id(&label, "probe-runtime"),
                                "Probe runtime",
                            ),
                            TaskItemState::pending(
                                build_task_step_id(&label, "build-artifact"),
                                "Build artifact",
                            ),
                            TaskItemState::pending(
                                build_task_step_id(&label, "package-artifact"),
                                "Package artifact",
                            ),
                        ])
                })
                .collect(),
            deploys: server_names
                .iter()
                .map(|server_name| {
                    TaskItemState::pending(deploy_target_task_id(server_name), server_name.clone())
                        .with_children(vec![
                            TaskItemState::pending(
                                deploy_task_step_id(server_name, "connecting"),
                                "Preflight",
                            ),
                            TaskItemState::pending(
                                deploy_task_step_id(server_name, "uploading"),
                                "Uploading",
                            ),
                            TaskItemState::pending(
                                deploy_task_step_id(server_name, "preparing"),
                                "Preparing",
                            ),
                            TaskItemState::pending(
                                deploy_task_step_id(server_name, "starting"),
                                "Starting",
                            ),
                        ])
                })
                .collect(),
            success_lines: Vec::new(),
            summary_line: None,
        };
        let tree = build_deploy_tree(&state);
        let session = should_use_deploy_task_tree().then(|| TaskTreeSession::new(tree));
        Self {
            state: Arc::new(Mutex::new(state)),
            session,
        }
    }

    pub(super) fn fail_preflight_check(&self, server_name: &str, detail: impl Into<String>) {
        let msg = detail.into();
        self.fail_deploy_step(server_name, "connecting", msg.clone());
        self.rename_deploy_step(server_name, "connecting", "Preflight failed");
        self.fail_deploy_target_without_detail(server_name);
        self.cancel_pending_deploy_children(server_name, "cancelled");
    }

    pub(super) fn mark_build_step_running(&self, target_label: &str, step: &str) {
        self.mark_running_by_id(&build_target_task_id(target_label));
        self.mark_running_by_id(&build_task_step_id(target_label, step));
    }

    pub(super) fn succeed_build_step(
        &self,
        target_label: &str,
        step: &str,
        detail: Option<String>,
    ) {
        self.complete_by_id(
            &build_task_step_id(target_label, step),
            detail,
            DeployCompletionKind::Succeeded,
        );
    }

    pub(super) fn skip_build_step(
        &self,
        target_label: &str,
        step: &str,
        detail: impl Into<String>,
    ) {
        self.complete_by_id(
            &build_task_step_id(target_label, step),
            Some(detail.into()),
            DeployCompletionKind::Skipped,
        );
    }

    pub(super) fn fail_build_step(
        &self,
        target_label: &str,
        step: &str,
        detail: impl Into<String>,
    ) {
        self.complete_by_id(
            &build_task_step_id(target_label, step),
            Some(detail.into()),
            DeployCompletionKind::Failed,
        );
    }

    pub(super) fn append_cached_artifact_step(&self, target_label: &str, detail: Option<String>) {
        let parent_id = build_target_task_id(target_label);
        let child_id = build_task_step_id(target_label, "use-cached-artifact");
        let mut state = self.state.lock().unwrap();
        let parent = find_task_mut(&mut state.builds, &parent_id)
            .unwrap_or_else(|| panic!("missing build task {parent_id}"));
        if parent.find(&child_id).is_none() {
            let mut child = TaskItemState::pending(child_id.clone(), "Use cached artifact");
            if let Some(detail) = &detail {
                child = child.with_detail(detail.clone());
            }
            parent.append_child(child);
        }
        self.refresh_locked(&state);
        drop(state);
        self.succeed_build_step(target_label, "use-cached-artifact", detail);
    }

    pub(super) fn succeed_build_target(&self, target_label: &str, detail: Option<String>) {
        self.complete_by_id(
            &build_target_task_id(target_label),
            detail,
            DeployCompletionKind::Succeeded,
        );
    }

    pub(super) fn mark_build_target_running(&self, target_label: &str) {
        self.mark_running_by_id(&build_target_task_id(target_label));
    }

    pub(super) fn fail_build_target(&self, target_label: &str, detail: impl Into<String>) {
        self.complete_by_id(
            &build_target_task_id(target_label),
            Some(detail.into()),
            DeployCompletionKind::Failed,
        );
    }

    pub(super) fn cancel_pending_build_children(&self, target_label: &str, reason: &str) {
        self.cancel_pending_children(&build_target_task_id(target_label), reason);
    }

    pub(super) fn mark_deploy_step_running(&self, server_name: &str, step: &str) {
        self.mark_running_by_id(&deploy_target_task_id(server_name));
        self.mark_running_by_id(&deploy_task_step_id(server_name, step));
    }

    pub(super) fn update_deploy_step_progress(
        &self,
        server_name: &str,
        step: &str,
        detail: String,
        progress: f64,
    ) {
        self.update_by_id(&deploy_task_step_id(server_name, step), |task| {
            task.detail = Some(detail);
            task.progress = Some(progress);
        });
    }

    pub(super) fn succeed_deploy_step(
        &self,
        server_name: &str,
        step: &str,
        detail: Option<String>,
    ) {
        self.complete_by_id(
            &deploy_task_step_id(server_name, step),
            detail,
            DeployCompletionKind::Succeeded,
        );
    }

    pub(super) fn skip_deploy_step(
        &self,
        server_name: &str,
        step: &str,
        detail: impl Into<String>,
    ) {
        self.complete_by_id(
            &deploy_task_step_id(server_name, step),
            Some(detail.into()),
            DeployCompletionKind::Skipped,
        );
    }

    pub(super) fn fail_deploy_step(
        &self,
        server_name: &str,
        step: &str,
        detail: impl Into<String>,
    ) {
        self.complete_by_id(
            &deploy_task_step_id(server_name, step),
            Some(detail.into()),
            DeployCompletionKind::Failed,
        );
    }

    /// Add a release-command sub-step under a server's `Preparing` task.
    /// `is_leader = true` shows the active "Running release command" row;
    /// `is_leader = false` shows the muted "Waiting for release command" row.
    pub(super) fn add_release_step(&self, server_name: &str, is_leader: bool) {
        let parent_id = deploy_task_step_id(server_name, "preparing");
        let child_id = deploy_task_step_id(server_name, "release");
        let label = if is_leader {
            "Running release command"
        } else {
            "Waiting for release command"
        };
        let mut state = self.state.lock().unwrap();
        let parent = find_task_mut(&mut state.deploys, &parent_id)
            .unwrap_or_else(|| panic!("missing preparing step for {server_name}"));
        if parent.find(&child_id).is_none() {
            parent.append_child(TaskItemState::pending(child_id, label));
        }
        self.refresh_locked(&state);
    }

    pub(super) fn mark_release_step_running(&self, server_name: &str) {
        self.mark_running_by_id(&deploy_task_step_id(server_name, "release"));
    }

    pub(super) fn succeed_release_step(&self, server_name: &str, detail: Option<String>) {
        self.complete_by_id(
            &deploy_task_step_id(server_name, "release"),
            detail,
            DeployCompletionKind::Succeeded,
        );
    }

    pub(super) fn fail_release_step(&self, server_name: &str, detail: impl Into<String>) {
        self.complete_by_id(
            &deploy_task_step_id(server_name, "release"),
            Some(detail.into()),
            DeployCompletionKind::Failed,
        );
    }

    pub(super) fn cancel_release_step(&self, server_name: &str, reason: impl Into<String>) {
        self.complete_by_id(
            &deploy_task_step_id(server_name, "release"),
            Some(reason.into()),
            DeployCompletionKind::Cancelled,
        );
    }

    pub(super) fn rename_deploy_step(&self, server_name: &str, step: &str, new_label: &str) {
        self.update_by_id(&deploy_task_step_id(server_name, step), |task| {
            task.label = new_label.to_string();
        });
    }

    pub(super) fn succeed_deploy_target(&self, server_name: &str, detail: Option<String>) {
        self.complete_by_id(
            &deploy_target_task_id(server_name),
            detail,
            DeployCompletionKind::Succeeded,
        );
    }

    pub(super) fn fail_deploy_target_without_detail(&self, server_name: &str) {
        self.complete_by_id(
            &deploy_target_task_id(server_name),
            None,
            DeployCompletionKind::Failed,
        );
    }

    pub(super) fn cancel_pending_deploy_children(&self, server_name: &str, reason: &str) {
        self.cancel_pending_children(&deploy_target_task_id(server_name), reason);
    }

    pub(super) fn abort_incomplete(&self, _reason: &str) {
        let mut state = self.state.lock().unwrap();
        abort_incomplete_tasks(&mut state.builds);
        abort_incomplete_tasks(&mut state.deploys);
        self.refresh_locked(&state);
    }

    pub(super) fn set_success_summary(
        &self,
        version: &str,
        routes: &[String],
        https_port: Option<u16>,
    ) {
        let mut state = self.state.lock().unwrap();
        state.success_lines =
            format_deploy_summary_lines_with_https_port("Release", version, routes, https_port);
        self.refresh_locked(&state);
    }

    pub(super) fn set_error_summary(&self, summary: String) {
        let mut state = self.state.lock().unwrap();
        state.summary_line = Some((summary, TreeTextTone::Error));
        self.refresh_locked(&state);
    }

    pub(super) fn finalize(&self) {
        if let Some(session) = &self.session {
            session.finalize();
        }
    }

    #[cfg(test)]
    pub(super) fn snapshot(&self) -> DeployTaskTreeState {
        self.state.lock().unwrap().clone()
    }

    fn mark_running_by_id(&self, id: &str) {
        self.update_by_id(id, |task| {
            if matches!(task.state, TaskState::Pending) {
                task.state = TaskState::Running {
                    started_at: Instant::now(),
                };
                task.detail = None;
            }
        });
    }

    fn complete_by_id(&self, id: &str, detail: Option<String>, kind: DeployCompletionKind) {
        self.update_by_id(id, |task| {
            let elapsed = match task.state {
                TaskState::Running { started_at } => Some(started_at.elapsed()),
                _ => None,
            };
            task.state = match kind {
                DeployCompletionKind::Succeeded => TaskState::Succeeded { elapsed },
                DeployCompletionKind::Skipped => TaskState::Skipped { elapsed },
                DeployCompletionKind::Failed => TaskState::Failed { elapsed },
                DeployCompletionKind::Cancelled => TaskState::Cancelled { elapsed },
            };
            task.detail = detail;
            task.progress = None;
        });
    }

    fn cancel_pending_children(&self, parent_id: &str, reason: &str) {
        let mut state = self.state.lock().unwrap();
        let parent = find_task_mut_in_state(&mut state, parent_id)
            .unwrap_or_else(|| panic!("missing parent task {parent_id}"));
        cancel_pending_children(parent, reason);
        self.refresh_locked(&state);
    }

    fn update_by_id<F>(&self, id: &str, update: F)
    where
        F: FnOnce(&mut TaskItemState),
    {
        let mut state = self.state.lock().unwrap();
        let task =
            find_task_mut_in_state(&mut state, id).unwrap_or_else(|| panic!("missing task {id}"));
        update(task);
        self.refresh_locked(&state);
    }

    fn refresh_locked(&self, state: &DeployTaskTreeState) {
        if let Some(session) = &self.session {
            session.set_tree(build_deploy_tree(state));
        }
    }
}

pub(super) fn should_use_deploy_task_tree() -> bool {
    output::is_pretty() && output::is_interactive()
}

fn abort_incomplete_tasks(tasks: &mut [TaskItemState]) {
    for task in tasks {
        abort_incomplete_task(task);
    }
}

fn abort_incomplete_task(task: &mut TaskItemState) {
    for child in &mut task.children {
        abort_incomplete_task(child);
    }

    let elapsed = match task.state {
        TaskState::Running { started_at } => Some(started_at.elapsed()),
        _ => None,
    };

    match task.state {
        TaskState::Pending | TaskState::Running { .. } => {
            task.state = TaskState::Cancelled { elapsed };
        }
        TaskState::Succeeded { .. }
        | TaskState::Failed { .. }
        | TaskState::Skipped { .. }
        | TaskState::Cancelled { .. } => {}
    }
}

pub(super) fn build_target_task_id(target_label: &str) -> String {
    format!("build:{target_label}")
}

pub(super) fn build_task_step_id(target_label: &str, step: &str) -> String {
    format!("build:{target_label}:{step}")
}

pub(super) fn deploy_target_task_id(server_name: &str) -> String {
    format!("deploy:{server_name}")
}

pub(super) fn deploy_task_step_id(server_name: &str, step: &str) -> String {
    format!("deploy:{server_name}:{step}")
}

/// Build the render tree from deploy state. This replaces the old UiNode-based
/// `build_deploy_task_tree_root`. Controllers call this via `refresh_locked()`.
pub(super) fn build_deploy_tree(state: &DeployTaskTreeState) -> Vec<TreeNode> {
    let mut tree = Vec::new();
    let has_deploys = !state.deploys.is_empty();

    // Build reporter
    match state.builds.as_slice() {
        [] => {}
        [build] => {
            let label = if matches!(build.state, TaskState::Succeeded { .. }) {
                "Built"
            } else {
                "Building"
            };
            tree.push(TreeNode::AccentTask(TaskItemState {
                id: build.id.clone(),
                label: label.to_string(),
                state: build.state.clone(),
                detail: build.detail.clone(),
                progress: None,
                children: vec![],
            }));
            if has_deploys {
                tree.push(TreeNode::Spacer);
            }
        }
        builds => {
            let group_state = aggregate_group_state(builds);
            tree.push(TreeNode::Task(TaskItemState {
                id: "build-group".into(),
                label: "Building".into(),
                state: group_state,
                detail: None,
                progress: None,
                children: builds
                    .iter()
                    .map(|b| TaskItemState {
                        id: b.id.clone(),
                        label: b.label.clone(),
                        state: b.state.clone(),
                        detail: b.detail.clone(),
                        progress: None,
                        children: vec![],
                    })
                    .collect(),
            }));
            if has_deploys {
                tree.push(TreeNode::Spacer);
            }
        }
    }

    // Deploy groups
    for (index, deploy) in state.deploys.iter().enumerate() {
        let label = match &deploy.state {
            TaskState::Succeeded { .. } => format!("Deployed to {}", deploy.label),
            TaskState::Failed { .. } => format!("Deploy to {} failed", deploy.label),
            _ => format!("Deploying to {}", deploy.label),
        };
        tree.push(TreeNode::Task(TaskItemState {
            id: deploy.id.clone(),
            label,
            state: deploy.state.clone(),
            detail: deploy.detail.clone(),
            progress: None,
            children: deploy.children.clone(),
        }));
        if index + 1 < state.deploys.len() {
            tree.push(TreeNode::Spacer);
        }
    }

    if !state.success_lines.is_empty() {
        if !tree.is_empty() && !matches!(tree.last(), Some(TreeNode::Spacer)) {
            tree.push(TreeNode::Spacer);
        }
        let max_label_width = state
            .success_lines
            .iter()
            .map(|l| l.label.len())
            .max()
            .unwrap_or(0);
        for line in &state.success_lines {
            let padded_label = if line.label.is_empty() {
                " ".repeat(max_label_width)
            } else {
                format!("{:<width$}", line.label, width = max_label_width)
            };
            tree.push(TreeNode::LabeledText {
                label: padded_label,
                value: line.value.clone(),
            });
        }
    }

    if let Some((summary, tone)) = &state.summary_line {
        if !tree.is_empty() && !matches!(tree.last(), Some(TreeNode::Spacer)) {
            tree.push(TreeNode::Spacer);
        }
        tree.push(TreeNode::Text {
            text: summary.clone(),
            tone: tone.clone(),
        });
    }

    tree
}

fn aggregate_group_state(tasks: &[TaskItemState]) -> TaskState {
    if let Some(started_at) = tasks.iter().find_map(|task| match &task.state {
        TaskState::Running { started_at } => Some(*started_at),
        _ => None,
    }) {
        return TaskState::Running { started_at };
    }

    if tasks
        .iter()
        .any(|task| matches!(task.state, TaskState::Failed { .. }))
    {
        return TaskState::Failed { elapsed: None };
    }

    let all_succeeded = tasks
        .iter()
        .all(|task| matches!(task.state, TaskState::Succeeded { .. }));

    if all_succeeded {
        TaskState::Succeeded { elapsed: None }
    } else {
        TaskState::Pending
    }
}

fn find_task_mut<'a>(tasks: &'a mut [TaskItemState], id: &str) -> Option<&'a mut TaskItemState> {
    tasks.iter_mut().find_map(|task| task.find_mut(id))
}

fn find_task_mut_in_state<'a>(
    state: &'a mut DeployTaskTreeState,
    id: &str,
) -> Option<&'a mut TaskItemState> {
    find_task_mut(&mut state.builds, id).or_else(|| find_task_mut(&mut state.deploys, id))
}

fn cancel_pending_children(parent: &mut TaskItemState, reason: &str) {
    for child in &mut parent.children {
        if matches!(child.state, TaskState::Pending) {
            child.state = TaskState::Cancelled { elapsed: None };
            child.detail = Some(reason.to_string());
        }
        cancel_pending_children(child, reason);
    }
}

pub(super) fn build_artifact_target_groups(
    server_targets: &[(String, ServerTarget)],
    use_unified_target_process: bool,
) -> Vec<ArtifactBuildGroup> {
    let unique_targets: Vec<String> = server_targets
        .iter()
        .map(|(_, target)| target.label())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    if unique_targets.is_empty() {
        return Vec::new();
    }

    if use_unified_target_process {
        let build_target_label = unique_targets[0].clone();
        return vec![ArtifactBuildGroup {
            build_target_label,
            cache_target_label: UNIFIED_JS_CACHE_TARGET_LABEL.to_string(),
            target_labels: unique_targets,
            display_target_label: None,
        }];
    }

    unique_targets
        .into_iter()
        .map(|label| ArtifactBuildGroup {
            build_target_label: label.clone(),
            cache_target_label: label.clone(),
            target_labels: vec![label.clone()],
            display_target_label: Some(label),
        })
        .collect()
}

#[cfg(test)]
mod tests;
