use super::super::agents::ASSETS;
use rmcp::model::{ContentBlock, Resource, Tool, ToolAnnotations};
use serde_json::{Map, Value, json};

pub(super) fn tools() -> Vec<Tool> {
    [
        ("tako_docs", "List bundled documentation and SDK skills, or read a topic. Topics include docs/quickstart, docs/cli, docs/tako-toml, docs/troubleshooting, and skills/tako-sdk-js. A bare docs topic such as quickstart also works.", json!({"topic":{"type":"string","description":"Omit to list all topics; otherwise use a topic from the list."}}), vec![]),
        ("tako_search_docs", "Search version-matched Tako documentation and language SDK skills offline. Returns matching lines with topic names and line numbers. Use tako_docs to read the full topic.", json!({"query":{"type":"string","minLength":1,"description":"Case-insensitive literal text to find."}}), vec!["query"]),
    ].into_iter().map(|(name, description, properties, required)| Tool::new(name, description,
        json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}).as_object().unwrap().clone())
        .with_annotations(ToolAnnotations::new().read_only(true).destructive(false).open_world(false))).collect()
}

pub(super) fn resources() -> Vec<Resource> {
    ASSETS
        .iter()
        .map(|(key, content)| {
            Resource::new(format!("tako://{key}"), *key)
                .with_mime_type("text/markdown")
                .with_size(content.len() as u64)
        })
        .collect()
}

pub(super) fn content(key: &str) -> Option<&'static str> {
    ASSETS
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, content)| *content)
}

fn topic_title(content: &str) -> Option<&str> {
    let mut lines = content.lines();
    if lines.next() == Some("---") {
        // Documentation uses single-line titles; skills use a body heading.
        for line in lines.take_while(|line| *line != "---") {
            if let Some(title) = line.strip_prefix("title:") {
                let title = title.trim().trim_matches(['"', '\'']);
                if !title.is_empty() {
                    return Some(title);
                }
            }
        }
    }
    content.lines().find_map(|line| line.strip_prefix("# "))
}

pub(super) fn call(
    name: &str,
    arguments: &Map<String, Value>,
) -> Result<Vec<ContentBlock>, String> {
    let key = if name == "tako_docs" {
        "topic"
    } else {
        "query"
    };
    if arguments.keys().any(|name| name != key) {
        return Err(format!("Only {key} is supported"));
    }
    let text = if name == "tako_docs" {
        match arguments.get("topic") {
            None => ASSETS
                .iter()
                .map(|(key, content)| match topic_title(content) {
                    Some(title) => format!("{key}: # {title}"),
                    None => key.to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Some(Value::String(topic)) => {
                let topic = topic.strip_prefix("tako://").unwrap_or(topic);
                content(topic)
                    .or_else(|| content(&format!("docs/{topic}")))
                    .ok_or_else(|| {
                        format!(
                            "Unknown topic {topic}. Call tako_docs with no topic to list topics"
                        )
                    })?
                    .to_string()
            }
            _ => return Err("topic must be a string".into()),
        }
    } else {
        let query = arguments
            .get("query")
            .and_then(Value::as_str)
            .filter(|query| !query.trim().is_empty())
            .ok_or("query must be a non-empty string")?
            .to_lowercase();
        let mut matches = Vec::new();
        for (key, content) in ASSETS {
            for (number, line) in content.lines().enumerate() {
                if line.to_lowercase().contains(&query) {
                    matches.push(format!(
                        "{key}:{}: {}",
                        number + 1,
                        line.chars().take(500).collect::<String>()
                    ));
                    if matches.len() == 50 {
                        matches.push(
                            "Showing the first 50 matches. Narrow the query to find more.".into(),
                        );
                        return Ok(vec![ContentBlock::text(matches.join("\n"))]);
                    }
                }
            }
        }
        if matches.is_empty() {
            "No matches. Call tako_docs to list topics.".into()
        } else {
            matches.join("\n")
        }
    };
    Ok(vec![ContentBlock::text(text)])
}
