//! Glob patterns for `KEYS`, `SCAN MATCH` and `HSCAN MATCH`, as Redis
//! matches them: `*`, `?`, `[...]` (with `^` and ranges) and `\` escapes.

/// Matching steps one command may spend, across every key it matches.
/// Backtracking over the last `*` makes one match cost up to
/// `pattern.len() * text.len()` steps, which long patterns and keys turn
/// into seconds of work; past this budget the command fails instead.
const BUDGET: u64 = 20_000_000;

pub(crate) struct Budget(u64);

impl Budget {
    pub(crate) fn new() -> Self {
        Self(BUDGET)
    }

    fn spend(&mut self, steps: usize) -> bool {
        match self.0.checked_sub(steps as u64) {
            Some(left) => {
                self.0 = left;
                true
            }
            None => {
                self.0 = 0;
                false
            }
        }
    }
}

/// The error a command replies with once its budget runs out.
pub(crate) const TOO_COMPLEX: &str = "ERR pattern is too complex to match; use a simpler pattern";

/// Whether `text` matches `pattern`, or `None` once `budget` runs out.
pub(crate) fn matches(pattern: &[u8], text: &[u8], budget: &mut Budget) -> Option<bool> {
    let (mut p, mut t) = (0, 0);
    // Where to resume after the most recent `*`: pattern after it, and the
    // text position it currently stands for.
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if !budget.spend(1) {
            return None;
        }
        if p < pattern.len() {
            match pattern[p] {
                b'*' => {
                    while p < pattern.len() && pattern[p] == b'*' {
                        p += 1;
                    }
                    if p == pattern.len() {
                        return Some(true);
                    }
                    star = Some((p, t));
                    continue;
                }
                b'?' => {
                    p += 1;
                    t += 1;
                    continue;
                }
                b'[' => {
                    let class = class(pattern, p, text[t]);
                    // A class costs the bytes it spans.
                    let span = class.map_or(pattern.len() - p, |(_, next)| next - p);
                    if !budget.spend(span) {
                        return None;
                    }
                    if let Some((true, next)) = class {
                        p = next;
                        t += 1;
                        continue;
                    }
                }
                b'\\' if p + 1 < pattern.len() => {
                    if pattern[p + 1] == text[t] {
                        p += 2;
                        t += 1;
                        continue;
                    }
                }
                c => {
                    if c == text[t] {
                        p += 1;
                        t += 1;
                        continue;
                    }
                }
            }
        }
        // Mismatch: let the last `*` absorb one more byte.
        match star {
            Some((after, at)) => {
                star = Some((after, at + 1));
                p = after;
                t = at + 1;
            }
            None => return Some(false),
        }
    }
    Some(pattern[p..].iter().all(|c| *c == b'*'))
}

/// Matches one byte against the class starting at `pattern[open] == '['`.
/// Returns whether it matched and where the pattern continues.
fn class(pattern: &[u8], open: usize, byte: u8) -> Option<(bool, usize)> {
    let mut i = open + 1;
    let negate = pattern.get(i) == Some(&b'^');
    if negate {
        i += 1;
    }
    let mut hit = false;
    loop {
        let c = *pattern.get(i)?;
        if c == b']' {
            break;
        }
        if c == b'\\' && i + 1 < pattern.len() {
            hit |= pattern[i + 1] == byte;
            i += 2;
        } else if i + 2 < pattern.len() && pattern[i + 1] == b'-' && pattern[i + 2] != b']' {
            let (mut start, mut end) = (c, pattern[i + 2]);
            if start > end {
                std::mem::swap(&mut start, &mut end);
            }
            hit |= (start..=end).contains(&byte);
            i += 3;
        } else {
            hit |= c == byte;
            i += 1;
        }
    }
    Some((hit != negate, i + 1))
}

#[cfg(test)]
mod tests {
    use super::{Budget, matches};

    #[test]
    fn matches_like_redis() {
        let cases: &[(&str, &str, bool)] = &[
            ("*", "", true),
            ("*", "anything", true),
            ("h?llo", "hello", true),
            ("h?llo", "hllo", false),
            ("h*llo", "heeeello", true),
            ("h[ae]llo", "hallo", true),
            ("h[ae]llo", "hillo", false),
            ("h[^e]llo", "hallo", true),
            ("h[^e]llo", "hello", false),
            ("h[a-b]llo", "hbllo", true),
            ("h[b-a]llo", "hallo", true),
            ("h\\*llo", "h*llo", true),
            ("h\\*llo", "hello", false),
            ("user:*:session", "user:42:session", true),
            ("user:*:session", "user:42:sessions", false),
            ("*a*b", "xaxxb", true),
            ("*a*b", "xaxxbc", false),
            ("a**", "a", true),
            ("[", "x", false),
            ("", "", true),
            ("", "a", false),
        ];
        for (pattern, text, expected) in cases {
            assert_eq!(
                matches(pattern.as_bytes(), text.as_bytes(), &mut Budget::new()),
                Some(*expected),
                "{pattern} ~ {text}"
            );
        }
    }

    #[test]
    fn expensive_patterns_run_out_of_budget() {
        let pattern = "*a".repeat(64) + "b";
        let text = "a".repeat(10_000);
        assert_eq!(
            matches(pattern.as_bytes(), text.as_bytes(), &mut Budget::new()),
            Some(false)
        );
        // `*` then a run that matches for 32 KiB at every position before
        // failing: about 32 Ki x 32 Ki steps for one key.
        let pattern = format!("*{}b", "a".repeat(32 * 1024));
        let text = "a".repeat(64 * 1024);
        assert_eq!(
            matches(pattern.as_bytes(), text.as_bytes(), &mut Budget::new()),
            None
        );
        // A class is charged for its length.
        let pattern = format!("[{}]", "a".repeat(30_000_000));
        assert_eq!(matches(pattern.as_bytes(), b"b", &mut Budget::new()), None);
    }
}
