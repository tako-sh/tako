use crate::output;

pub(super) async fn configure_server(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    if !output::is_interactive() {
        return Err("Interactive server configuration requires a terminal.".into());
    }

    let choice = output::select(
        "Configure",
        None,
        vec![
            ("DNS wildcard certificates".to_string(), "dns".to_string()),
            (
                "Source IP behind a trusted proxy".to_string(),
                "source-ip".to_string(),
            ),
        ],
    )?;

    match choice.as_str() {
        "dns" => super::dns::configure_dns(name).await,
        "source-ip" => super::trusted_proxy::configure_trusted_proxy(name).await,
        _ => Err(output::operation_cancelled_error().into()),
    }
}
