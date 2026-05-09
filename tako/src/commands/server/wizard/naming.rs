pub(super) fn append_unique_suggestions(target: &mut Vec<String>, source: &[String]) {
    for value in source {
        push_unique_suggestion(target, value.clone());
    }
}

pub(super) fn push_unique_suggestion(values: &mut Vec<String>, value: String) {
    if value.is_empty() {
        return;
    }
    if values.iter().any(|existing| existing == &value) {
        return;
    }
    values.push(value);
}

pub(super) fn default_server_name_from_host(host: &str) -> Option<String> {
    let label = host
        .trim()
        .trim_end_matches('.')
        .split('.')
        .next()
        .unwrap_or("")
        .trim();
    if is_valid_default_server_name(label) {
        Some(label.to_string())
    } else {
        None
    }
}

pub(super) fn next_available_server_name(
    base: &str,
    servers: &crate::config::ServersToml,
) -> String {
    for index in 2.. {
        let suffix = format!("-{index}");
        let max_base_len = 63usize.saturating_sub(suffix.len());
        let trimmed_base = base
            .chars()
            .take(max_base_len)
            .collect::<String>()
            .trim_end_matches('-')
            .to_string();
        let candidate = format!("{trimmed_base}{suffix}");
        if !servers.contains(&candidate) {
            return candidate;
        }
    }

    unreachable!()
}

fn is_valid_default_server_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 63 || name.ends_with('-') {
        return false;
    }
    name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub(super) fn record_server_history(host: &str, name: &str, port: u16) {
    let mut history = crate::config::CliHistoryToml::load().unwrap_or_default();
    history.record_server_prompt_values(host, name, port);
    if let Err(e) = history.save() {
        tracing::warn!("Could not save CLI history: {e}");
    }
}
