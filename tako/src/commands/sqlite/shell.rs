//! `tako sqlite shell`: run SQL against a hosted database.

use std::io::{BufRead, Write};

use tako_core::{Command, DatabaseQueryResult};

use super::send;
use super::target::Home;
use crate::output;

const CELL_WIDTH: usize = 60;

pub(super) async fn run(
    home: &Home,
    id: &str,
    name: &str,
    command: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(sql) = command {
        return execute(home, id, &sql).await;
    }
    let interactive = output::is_interactive();
    if interactive {
        output::hint(&format!(
            "Connected to {name} on {}. End statements with `;`. `.tables`, `.schema [table]`, `.quit`.",
            home.name
        ));
    }
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut buffer = String::new();
    loop {
        if interactive {
            eprint!(
                "{}",
                if buffer.is_empty() {
                    format!("{name}> ")
                } else {
                    "   ...> ".into()
                }
            );
            std::io::stderr().flush()?;
        }
        let Some(line) = lines.next() else { break };
        let line = line?;
        if buffer.is_empty() {
            let trimmed = line.trim();
            match trimmed.split_whitespace().next() {
                Some(".quit" | ".exit") => break,
                Some(".tables") => {
                    report(execute(home, id, "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name").await);
                    continue;
                }
                Some(".schema") => {
                    let sql = match trimmed.split_whitespace().nth(1) {
                        Some(table) => format!(
                            "SELECT sql FROM sqlite_schema WHERE tbl_name = '{}' AND sql IS NOT NULL",
                            table.replace('\'', "''")
                        ),
                        None => "SELECT sql FROM sqlite_schema WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'".into(),
                    };
                    report(execute(home, id, &sql).await);
                    continue;
                }
                Some(command) if command.starts_with('.') => {
                    output::warning(&format!("Unknown command {command}"));
                    continue;
                }
                None => continue,
                _ => {}
            }
        }
        buffer.push_str(&line);
        buffer.push('\n');
        if is_complete(&buffer) {
            let sql = std::mem::take(&mut buffer);
            report(execute(home, id, sql.trim()).await);
        }
    }
    if !buffer.trim().is_empty() {
        report(execute(home, id, buffer.trim()).await);
    }
    Ok(())
}

fn report(result: Result<(), Box<dyn std::error::Error>>) {
    if let Err(error) = result {
        output::error_stderr(&error.to_string());
    }
}

/// Whether `sql` ends with a complete statement, by SQLite's own rules.
fn is_complete(sql: &str) -> bool {
    let Ok(text) = std::ffi::CString::new(sql) else {
        return true;
    };
    // SAFETY: `text` is a valid NUL-terminated string for the call.
    unsafe { rusqlite::ffi::sqlite3_complete(text.as_ptr()) != 0 }
}

async fn execute(home: &Home, id: &str, sql: &str) -> Result<(), Box<dyn std::error::Error>> {
    let result: DatabaseQueryResult = send(
        home,
        Command::QueryDatabase {
            id: id.to_owned(),
            sql: sql.to_owned(),
        },
    )
    .await?;
    if output::is_json() {
        println!("{}", serde_json::to_string(&result)?);
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if result.columns.is_empty() {
        writeln!(out, "{} row(s) changed", result.affected_row_count)?;
        return Ok(());
    }
    let cells: Vec<Vec<String>> = result
        .rows
        .iter()
        .map(|row| row.iter().map(cell).collect())
        .collect();
    let mut widths: Vec<usize> = result.columns.iter().map(|c| c.chars().count()).collect();
    for row in &cells {
        for (width, value) in widths.iter_mut().zip(row) {
            *width = (*width).max(value.chars().count());
        }
    }
    let line = |values: &[String]| {
        values
            .iter()
            .zip(&widths)
            .map(|(value, width)| format!("{value:<width$}"))
            .collect::<Vec<_>>()
            .join(" │ ")
    };
    writeln!(out, "{}", line(&result.columns))?;
    writeln!(
        out,
        "{}",
        widths
            .iter()
            .map(|w| "─".repeat(*w))
            .collect::<Vec<_>>()
            .join("─┼─")
    )?;
    for row in &cells {
        writeln!(out, "{}", line(row))?;
    }
    if result.truncated {
        writeln!(
            out,
            "(showing the first {} rows)",
            tako_core::DATABASE_QUERY_MAX_ROWS
        )?;
    }
    Ok(())
}

fn cell(value: &serde_json::Value) -> String {
    let text = match value {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Object(blob) => match blob.get("base64").and_then(|b| b.as_str()) {
            Some(base64) => format!("<blob {} bytes>", base64.len() * 3 / 4),
            None => value.to_string(),
        },
        other => other.to_string(),
    };
    let text = text.replace('\n', "⏎");
    if text.chars().count() > CELL_WIDTH {
        let mut cut: String = text.chars().take(CELL_WIDTH - 1).collect();
        cut.push('…');
        cut
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_complete_by_sqlite_rules() {
        assert!(is_complete("SELECT 1;"));
        assert!(!is_complete("SELECT 1"));
        assert!(!is_complete("SELECT ';"));
        assert!(is_complete(
            "CREATE TRIGGER t AFTER INSERT ON x BEGIN SELECT 1; END;"
        ));
        assert!(!is_complete(
            "CREATE TRIGGER t AFTER INSERT ON x BEGIN SELECT 1;"
        ));
    }

    #[test]
    fn cells_are_readable_and_bounded() {
        assert_eq!(cell(&serde_json::Value::Null), "NULL");
        assert_eq!(
            cell(&serde_json::json!({"base64": "AAAA"})),
            "<blob 3 bytes>"
        );
        assert_eq!(cell(&serde_json::json!("a\nb")), "a⏎b");
        assert_eq!(
            cell(&serde_json::json!("x".repeat(100))).chars().count(),
            CELL_WIDTH
        );
    }
}
