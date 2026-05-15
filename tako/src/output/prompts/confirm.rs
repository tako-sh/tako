use std::io;

use console::Term;

use super::super::{is_interactive, is_pretty, theme_accent, theme_muted};
use super::prompt_escape_action;
use super::{
    EscapeAction, format_key_hints, format_pretty_cancelled_prompt,
    format_pretty_confirm_completion, format_pretty_confirm_prompt_with_description,
    pretty_prompt_input_column, wizard_back_error,
};

pub fn confirm(prompt: &str, default: bool) -> io::Result<bool> {
    confirm_with_description(prompt, None, default)
}

pub fn confirm_with_description(
    prompt: &str,
    description: Option<&str>,
    default: bool,
) -> io::Result<bool> {
    confirm_inner(prompt, description, default, false)
}

pub(in crate::output) fn confirm_with_description_back(
    prompt: &str,
    description: Option<&str>,
    default: bool,
) -> io::Result<bool> {
    confirm_inner(prompt, description, default, true)
}

fn confirm_inner(
    prompt: &str,
    description: Option<&str>,
    default: bool,
    show_back: bool,
) -> io::Result<bool> {
    if !is_interactive() {
        return Ok(default);
    }

    // Verbose mode: transcript-style confirm (still interactive, no screen erasing).
    if !is_pretty() {
        use crossterm::{
            cursor,
            event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
            terminal,
        };
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        // CodeQL[rust/cleartext-logging]: confirm prompts are user-facing stderr text;
        // secret flows pass generic copy that omits secret values and names.
        eprint!("{} {} ", theme_accent(prompt), theme_muted(hint));
        let _ = std::io::Write::flush(&mut io::stderr());
        terminal::enable_raw_mode()?;
        let _ = crossterm::execute!(io::stderr(), cursor::Show);
        let result = loop {
            if let Event::Key(KeyEvent {
                code, modifiers, ..
            }) = event::read()?
            {
                match code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        eprintln!("yes");
                        break Ok(true);
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') => {
                        eprintln!("no");
                        break Ok(false);
                    }
                    KeyCode::Enter => {
                        eprintln!("{}", if default { "yes" } else { "no" });
                        break Ok(default);
                    }
                    KeyCode::Esc => {
                        if prompt_escape_action(show_back) == EscapeAction::Back {
                            break Err(wizard_back_error());
                        }
                        continue;
                    }
                    KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                        break Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "Operation interrupted",
                        ));
                    }
                    _ => {}
                }
            }
        };
        terminal::disable_raw_mode()?;
        return result;
    }

    // Pretty mode: diamond-style confirm with crossterm
    use crossterm::{
        cursor,
        event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
        terminal,
    };

    let term = Term::stderr();

    let prompt_lines = format_pretty_confirm_prompt_with_description(prompt, description, default);
    for line in &prompt_lines {
        eprintln!("{line}");
    }
    // Key hints below the input line
    eprintln!("{}", format_key_hints(show_back));
    // Move cursor back to the › line
    let _ = crossterm::execute!(
        io::stderr(),
        cursor::MoveUp(2),
        cursor::MoveToColumn(pretty_prompt_input_column(true, false))
    );
    let _ = std::io::Write::flush(&mut io::stderr());

    // Raw mode: read single keypress
    terminal::enable_raw_mode()?;
    let _ = crossterm::execute!(io::stderr(), cursor::Show);
    let result = loop {
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event::read()?
        {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    break Ok(true);
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    break Ok(false);
                }
                KeyCode::Enter => {
                    break Ok(default);
                }
                KeyCode::Esc => {
                    if prompt_escape_action(show_back) == EscapeAction::Back {
                        break Err(wizard_back_error());
                    }
                    continue;
                }
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "Operation interrupted",
                    ));
                }
                _ => continue,
            }
        }
    };
    terminal::disable_raw_mode()?;

    // Move cursor below the key hints line so clear_last_lines counts correctly.
    let _ = crossterm::execute!(io::stderr(), cursor::MoveDown(1), cursor::MoveToColumn(0));
    let _ = std::io::Write::write_all(&mut io::stderr(), b"\r\n");
    let _ = std::io::Write::flush(&mut io::stderr());

    let is_cancelled = result
        .as_ref()
        .is_err_and(|e| e.kind() == io::ErrorKind::Interrupted && !super::is_wizard_back(e));

    let total_rows = 3 + description.is_some() as usize;
    let _ = term.clear_last_lines(total_rows);
    match &result {
        Ok(answer) => {
            let answer_text = if *answer { "yes" } else { "no" };
            for line in format_pretty_confirm_completion(prompt, default, answer_text) {
                let _ = term.write_line(&line);
            }
        }
        Err(_) if is_cancelled => {
            for line in format_pretty_cancelled_prompt(prompt) {
                let _ = term.write_line(&line);
            }
        }
        Err(_) => {}
    }

    result
}

#[cfg(test)]
mod tests {
    use super::super::super::is_interactive;
    use super::*;

    #[test]
    fn confirm_returns_default_in_non_tty_context() {
        if is_interactive() {
            return;
        }
        let answer = confirm("Proceed?", false).unwrap();
        assert!(!answer);
    }
}
