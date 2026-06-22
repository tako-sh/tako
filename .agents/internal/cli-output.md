---
name: cli-output
description: "Rules and patterns for Tako CLI output across normal, --verbose, and --ci modes. Read before changing CLI user-facing output in the `tako/` crate."
---

# Tako CLI Output

Use the shared output helpers. Human output goes to stderr. Structured JSON/data goes to stdout.

## Modes

Tako has two output systems:

- **Pretty output**: normal mode. Uses `output::info()`, task trees, spinners, prompts, colors, and symbols.
- **Tracing**: `--verbose` and `--ci`. Uses `tracing::*` and `output::timed()`.

Only one system renders at a time. Pretty-only helpers are no-ops in verbose/CI unless noted in the helper table.

### Normal

- Interactive, styled output.
- Tracing has no subscriber and does not render.
- Use task trees for multi-step flows that know their work up front.
- Use spinners for one-off work.
- Use diamond prompts for interactive questions.

### Verbose

- Transcript-style logs: `HH:MM:SS.mmm LEVEL message`.
- Do not render future/pending work.
- Prompts remain interactive but print as plain prompt text, not tracing records.

### CI

- Same behavior as verbose, without ANSI color or timestamps.
- Prompts use defaults or fail non-interactively.

## Text Helpers

| Helper | Normal | Verbose/CI |
| --- | --- | --- |
| `section(title)` | blank line + bold accent title, padded | no-op |
| `heading(title)` | blank line + bold title, padded | no-op |
| `info(message)` | default text, padded | no-op |
| `line(message)` | default text, no padding | no-op |
| `bullet(message)` | `  - message` | no-op |
| `success(message)` | `✔ message` | `tracing::info!` |
| `success_with_elapsed(message, elapsed)` | `✔ message elapsed` | `tracing::info!` |
| `warning(message)` | `! message` | no-op |
| `error(message)` / `error_block(message)` | red error text | `tracing::error!` |
| `muted(message)` | dim text, padded | no-op |
| `hint(message)` | dim text, padded | `tracing::info!` |

In interactive pretty mode, `info`, `muted`, `hint`, `section`, and `heading` add two spaces so plain text aligns with symbol-prefixed lines. Do not add manual padding. Use `line()` for isolated summary lines that should be flush-left.

## Formatting

- `strong(value)`: bold, no color. Use for important values like app names, server names, and versions.
- `accent(value)`: accent color, no bold. Use sparingly.
- `theme_success`, `theme_warning`, `theme_error`, `theme_muted`, `theme_dim`: low-level styling helpers.
- Do not pass ANSI-formatted strings to tracing.

Pretty elapsed times do not use parentheses:

```text
✔ Deployed 12s
✔ Downloaded 3s, 72 MB
```

TRACE timing records do use parentheses:

```text
TRACE SSH connect (250ms)
```

## Spinners

Spinners are pretty UI only. They are silent in verbose/CI except for errors.

- Wrap meaningful work in `output::timed(label)` so verbose/CI has timing.
- Use `with_spinner()` / `with_spinner_async()` for one operation with a success line.
- Use `with_spinner_async_err()` when the failure label should differ from the loading label.
- Use `with_spinner_async_simple()` when the operation should not print a success line.
- Use `with_spinner_silent()` for preflight checks where only failures matter.
- Use `PhaseSpinner` for broad phases such as building or log streaming.
- Use `TrackedSpinner` for progress like “Syncing secrets to N servers”.

Example:

```rust
let _t = output::timed("Validate config");
output::with_spinner("Validating", "Validated", || {
    validate()?;
    Ok(())
})?;
```

Normal:

```text
◧ Validating…
✔ Validated 1.2s
```

Verbose:

```text
TRACE Validate config (1.2s)
```

## Task Trees

Use persistent task trees for multi-step interactive flows such as deploy and upgrade.

Core rules:

- A **task** is a status-bearing parent row.
- A **sub task** is a single actionable step.
- Pretty mode may render the known tree up front.
- Verbose/CI must stay transcript-style and must not show pending future work.
- Running leaf rows use `◧ ◨ ◩ ◪`.
- Pending leaf rows use muted `○`; boxed pending rows use muted `□`.
- Succeeded state rows use `✔`; failed state rows use `✘`.
- Boxed rows use `■` for success and failure.
- Parent rows with children do not render their own icon or elapsed time.
- Leaf elapsed time uses a fixed two-space gap after label/detail.
- Completed rows stay visible for the life of the command.
- Child rows keep their own state styling when the parent finishes.
- Cancelled and skipped rows keep their labels, reuse the muted pending icon, and mute the whole row.
- Do not use strikethrough.

Ctrl-C behavior:

- Append exactly one blank line and `Operation cancelled`.
- Change currently running task rows to cancelled state so they stop animating and stop using live/accent color.
- Keep task labels unchanged. Do not synthesize labels like `Upload cancelled`.
- Leave pending rows pending and completed rows completed.

Deploy-specific conventions:

- Show `Connecting`, `Building`, and one `Deploying to <server>` task per target server.
- Start `Connecting` and `Building` together once planning is complete.
- Add a blank line after each top-level phase, not between sub tasks.
- If connection or build fails, abort remaining incomplete rows instead of leaving them pending.
- Do not keep decorative plan boxes or startup metadata in the live task tree.

## Prompts

Prompts work only in interactive mode. CI uses defaults or errors.

Prompts are not log lines. In verbose mode, print prompt text directly; do not wrap prompts in tracing.

Active prompt:

```text
◆ App name
  › myapp_
  enter submit
```

Completed prompt:

```text
◇ App name
  myapp
```

Cancelled prompt summary:

```text
◇ App name
```

Rules:

- Use the shared diamond prompt style.
- Use `◆` for active labels and `◇` for completed/cancelled summaries.
- Put descriptions, hints, warnings, and validation errors inside the prompt body as indented plain text.
- Do not add `!` or `✘` chrome inside prompt bodies.
- Do not use strikethrough for cancelled prompts.
- Confirm prompts keep `[Y/n]` or `[y/N]` on the label line.
- Select prompts use `enter select` and indented options.

## Tracing

Use tracing only for verbose/CI output. Tracing is no-op in normal mode.

Levels:

- `TRACE`: noisy detail and timing spans.
- `DEBUG`: meaningful internal steps such as connections, paths, sizes, versions.
- `INFO`: user-visible milestones; rarely needed directly because helpers often bridge to info.
- `WARN`: non-fatal issues.
- `ERROR`: failures.

Use `[name]` prefixes for per-target context:

```rust
tracing::debug!("[{name}] Uploading {size} to {path}");
```

Do not use tracing structured fields for CLI output.

### `output::timed()`

`output::timed(label)` owns start/finish tracing for meaningful work:

- Always emits a TRACE finish record on drop.
- Emits a DEBUG start record only if the action lasts at least 2 seconds.
- Adds the trailing `…` to the deferred start record automatically.

```rust
let _t = output::timed(&format!("[{name}] Upload artifact ({} bytes)", size));
```

Do not manually pair a start `debug!` with a finish log for the same action.

## Copy Rules

- Keep copy short, specific, and action-oriented.
- Prefer user verbs: `Set`, `Updated`, `Replace`, `Remove`, `Try again`.
- Avoid redundant context when the command, task, or prompt already names the object.
- Print environment context only when it changes how the user should interpret the operation.
- Use `warning()` for environment context before destructive operations; use `hint()` or fold context into the main sentence for read-only context.
- Do not nest `strong()` or `accent()` inside `warning()` text.
- URLs must remain literal contiguous `https://...` strings. Do not split, truncate, or replace them with labels.

## Anti-Patterns

- Ad-hoc ANSI escape codes.
- `println!` for human-facing output.
- Spinners without `timed()` around meaningful remote or slow work.
- Multiple result lines for one spinner.
- Manually padding `info()` or `hint()` output.
- Parentheses around pretty elapsed times.
- Pre-rendering future work in verbose/CI.
- Passing styled text to tracing.
- Using `strip_ansi` to clean messages.
- Prompt output through tracing.
- Strikethrough for cancelled UI.
- Decorative boxes where task trees already explain the work.
