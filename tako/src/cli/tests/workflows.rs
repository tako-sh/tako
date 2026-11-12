use super::*;

#[test]
fn workflows_list_parses_filters() {
    let cli = Cli::try_parse_from([
        "tako",
        "workflows",
        "ls",
        "--env",
        "staging",
        "--status",
        "dead",
        "--name",
        "send-email",
        "--limit",
        "200",
    ])
    .unwrap();

    let Some(Commands::Workflows(WorkflowCommands::List {
        env,
        server,
        status,
        name,
        limit,
    })) = cli.command
    else {
        panic!("expected Workflows::List");
    };
    assert_eq!(env.as_deref(), Some("staging"));
    assert!(server.is_none());
    assert_eq!(status, Some(RunStatusArg::Dead));
    assert_eq!(name.as_deref(), Some("send-email"));
    assert_eq!(limit, Some(200));
}

#[test]
fn workflows_list_rejects_unknown_status_and_out_of_range_limit() {
    let error = Cli::try_parse_from(["tako", "workflows", "list", "--status", "failed"])
        .err()
        .expect("unknown status");
    let message = error.to_string();
    assert!(message.contains("invalid value 'failed'"), "{message}");
    assert!(
        message.contains("pending, running, succeeded, cancelled, dead"),
        "{message}"
    );

    for limit in ["0", "201"] {
        let error = Cli::try_parse_from(["tako", "workflows", "list", "--limit", limit])
            .err()
            .expect("out of range");
        assert!(error.to_string().contains("1..=200"), "{error}");
    }
}

#[test]
fn workflows_run_commands_require_a_run_id() {
    for action in ["show", "retry", "cancel"] {
        assert!(Cli::try_parse_from(["tako", "workflows", action]).is_err());
    }
    let cli = Cli::try_parse_from([
        "tako",
        "workflows",
        "cancel",
        "run_1",
        "--server",
        "la",
        "--yes",
    ])
    .unwrap();
    let Some(Commands::Workflows(WorkflowCommands::Cancel {
        run_id,
        env,
        server,
        yes,
    })) = cli.command
    else {
        panic!("expected Workflows::Cancel");
    };
    assert_eq!(run_id, "run_1");
    assert!(env.is_none());
    assert_eq!(server.as_deref(), Some("la"));
    assert!(yes);
}
