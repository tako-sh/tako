use super::super::wizard::push_history_if_interactive;

#[test]
fn push_history_if_interactive_records_prompted_steps() {
    let mut step_history = vec![0, 1];
    push_history_if_interactive(&mut step_history, 2, true);
    assert_eq!(step_history, vec![0, 1, 2]);
}

#[test]
fn push_history_if_interactive_skips_auto_derived_steps() {
    let mut step_history = vec![0, 1];
    push_history_if_interactive(&mut step_history, 2, false);
    assert_eq!(step_history, vec![0, 1]);
}
