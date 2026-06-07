use super::*;

#[test]
fn collect_process_tree_pids_includes_descendants() {
    let root = Pid::from_u32(10);
    let child = Pid::from_u32(11);
    let grandchild = Pid::from_u32(12);
    let unrelated = Pid::from_u32(99);
    let got = collect_process_tree_pids(
        &[
            (root, None),
            (child, Some(root)),
            (grandchild, Some(child)),
            (unrelated, None),
        ],
        root,
    );
    assert!(got.contains(&root));
    assert!(got.contains(&child));
    assert!(got.contains(&grandchild));
    assert!(!got.contains(&unrelated));
}

#[test]
fn collect_process_tree_pids_handles_parent_cycle() {
    let root = Pid::from_u32(1);
    let child = Pid::from_u32(2);
    let got = collect_process_tree_pids(&[(root, Some(child)), (child, Some(root))], root);
    assert_eq!(got.len(), 2);
}
