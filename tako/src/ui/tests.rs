use std::time::{Duration, Instant};

use super::session::append_interrupt_message;
use super::*;

#[test]
fn render_plain_task_states() {
    let tree = vec![TreeNode::Task(TaskItemState {
        id: "group".into(),
        label: "Checks".into(),
        state: TaskState::Pending,
        icon: TaskIcon::State,
        detail: None,
        progress: None,
        children: vec![
            TaskItemState {
                id: "a".into(),
                label: "prod-a".into(),
                state: TaskState::Succeeded {
                    elapsed: Some(Duration::from_secs(2)),
                },
                icon: TaskIcon::State,
                detail: None,
                progress: None,
                children: vec![],
            },
            TaskItemState {
                id: "b".into(),
                label: "prod-b".into(),
                state: TaskState::Failed {
                    elapsed: Some(Duration::from_secs(1)),
                },
                icon: TaskIcon::State,
                detail: Some("boom".into()),
                progress: None,
                children: vec![],
            },
            TaskItemState {
                id: "c".into(),
                label: "prod-c".into(),
                state: TaskState::Skipped { elapsed: None },
                icon: TaskIcon::State,
                detail: Some("skipped".into()),
                progress: None,
                children: vec![],
            },
        ],
    })];

    let lines = render_plain_lines(&tree);
    assert_eq!(lines[0], "Checks…");
    assert_eq!(lines[1], "  ✔ prod-a  2.0s");
    assert_eq!(lines[2], "  ✘ prod-b  1.0s");
    assert_eq!(lines[3], "    boom");
    assert_eq!(lines[4], "  ○ prod-c… skipped");
}

#[test]
fn render_plain_accent_task() {
    let tree = vec![
        TreeNode::AccentTask(TaskItemState {
            id: "build".into(),
            label: "Built".into(),
            state: TaskState::Succeeded {
                elapsed: Some(Duration::from_millis(3400)),
            },
            icon: TaskIcon::State,
            detail: Some("1.04 MB".into()),
            progress: None,
            children: vec![],
        }),
        TreeNode::Spacer,
    ];

    let lines = render_plain_lines(&tree);
    assert_eq!(lines[0], "✔ Built 1.04 MB  3.4s");
    assert_eq!(lines[1], "");
}

#[test]
fn succeeded_parent_keeps_success_icons_on_succeeded_children() {
    let tree = vec![TreeNode::Task(TaskItemState {
        id: "deploy".into(),
        label: "Deployed to prod-a".into(),
        state: TaskState::Succeeded {
            elapsed: Some(Duration::from_secs(23)),
        },
        icon: TaskIcon::State,
        detail: None,
        progress: None,
        children: vec![
            TaskItemState {
                id: "pre".into(),
                label: "Preflight".into(),
                state: TaskState::Succeeded {
                    elapsed: Some(Duration::from_millis(4800)),
                },
                icon: TaskIcon::State,
                detail: None,
                progress: None,
                children: vec![],
            },
            TaskItemState {
                id: "up".into(),
                label: "Uploaded".into(),
                state: TaskState::Succeeded {
                    elapsed: Some(Duration::from_millis(4100)),
                },
                icon: TaskIcon::State,
                detail: None,
                progress: None,
                children: vec![],
            },
        ],
    })];

    let lines = render_plain_lines(&tree);
    assert_eq!(lines[0], "Deployed to prod-a");
    assert_eq!(lines[1], "  ✔ Preflight  4.8s");
    assert_eq!(lines[2], "  ✔ Uploaded  4.1s");
}

#[test]
fn group_rows_hide_icons_while_box_children_show_progress_state() {
    let tree = vec![TreeNode::Task(
        TaskItemState::pending("deploy", "Deploying to prod-a")
            .with_icon(TaskIcon::None)
            .with_children(vec![
                TaskItemState::pending("preflight", "Preflight").with_icon(TaskIcon::Box),
                TaskItemState {
                    id: "uploading".into(),
                    label: "Uploading".into(),
                    state: TaskState::Running {
                        started_at: Instant::now(),
                    },
                    icon: TaskIcon::Box,
                    detail: None,
                    progress: None,
                    children: vec![],
                },
            ]),
    )];

    let lines = render_plain_lines(&tree);
    assert_eq!(lines[0], "Deploying to prod-a…");
    assert_eq!(lines[1], "  □ Preflight…");
    assert!(lines[2].starts_with("  ◧ Uploading…"));
}

#[test]
fn append_interrupt_message_adds_blank_line_and_error_text() {
    let mut tree = vec![TreeNode::Task(TaskItemState::pending(
        "deploy",
        "Deploying",
    ))];
    append_interrupt_message(&mut tree, "Operation cancelled");

    let lines = render_plain_lines(&tree);
    assert_eq!(
        lines,
        vec![
            "○ Deploying…".to_string(),
            String::new(),
            "Operation cancelled".to_string()
        ]
    );
}

#[test]
fn append_interrupt_message_cancels_running_tasks() {
    let mut tree = vec![TreeNode::Task(TaskItemState {
        id: "deploy".into(),
        label: "Deploying".into(),
        state: TaskState::Running {
            started_at: Instant::now(),
        },
        icon: TaskIcon::State,
        detail: None,
        progress: None,
        children: vec![
            TaskItemState {
                id: "a".into(),
                label: "Connected".into(),
                state: TaskState::Succeeded {
                    elapsed: Some(Duration::from_secs(1)),
                },
                icon: TaskIcon::State,
                detail: None,
                progress: None,
                children: vec![],
            },
            TaskItemState {
                id: "b".into(),
                label: "Starting".into(),
                state: TaskState::Running {
                    started_at: Instant::now(),
                },
                icon: TaskIcon::State,
                detail: None,
                progress: None,
                children: vec![],
            },
        ],
    })];
    append_interrupt_message(&mut tree, "Operation cancelled");

    let lines = render_plain_lines(&tree);
    assert!(lines[0].starts_with("Deploying…"));
    assert!(lines[1].starts_with("  ✔ Connected"));
    assert!(lines[2].starts_with("  ○ Starting…"));
    assert_eq!(lines[3], "");
    assert_eq!(lines[4], "Operation cancelled");
}

#[test]
fn task_item_find_and_find_mut() {
    let mut root = TaskItemState::pending("root", "Root").with_children(vec![
        TaskItemState::pending("child-a", "A"),
        TaskItemState::pending("child-b", "B"),
    ]);

    assert!(root.find("child-a").is_some());
    assert!(root.find("missing").is_none());

    let child = root.find_mut("child-b").unwrap();
    child.label = "Updated".into();
    assert_eq!(root.find("child-b").unwrap().label, "Updated");
}
