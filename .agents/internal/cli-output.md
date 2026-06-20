---
name: cli-output
description: "Rules and patterns for Tako CLI output across normal, --verbose, and --ci modes. Use this skill whenever writing or modifying any Tako CLI command output — including print statements, spinners, log lines, prompts, progress indicators, or error messages in the `tako/` crate. Also use when adding new commands, reviewing output consistency, or fixing output bugs. Triggers on any work touching `tako/src/output/`, `tako/src/commands/`, or CLI user-facing text."
---

# Tako CLI Output

Two output systems coexist in code. Only one renders at a time based on the mode.

## Architecture

- **Pretty output** (`output::info()`, spinners, prompts, etc.) — renders in normal mode. Most pretty-only text helpers are no-ops in verbose/CI, while outcome/error helpers bridge to tracing where noted below.
- **Tracing** (`tracing::debug!()`, `tracing::info!()`, etc.) — renders in verbose/CI mode, no-op in normal mode (no subscriber installed).
- **`output::timed(label)`** — the single source of truth for action tracking in verbose/CI. Emits a deferred DEBUG start log only if the action exceeds 2s, and always emits a TRACE end log on drop with elapsed time. Spinners do NOT emit tracing on their own — they are pure UI in pretty mode, and silent in verbose/CI. Wrap meaningful work in `timed()` regardless of whether a spinner is attached.

Both systems are called side-by-side in command code. Each is invisible in the mode it doesn't belong to.

## The Three Modes

### Normal (default, interactive terminal)

Pretty output renders — persistent task lists, spinners where still applicable, colors, symbols, diamond prompts. Tracing calls are no-ops (no subscriber installed).

- Colors via the brand palette
- Persistent task lists for multi-step interactive flows
- Show the whole known plan up front when the command already knows future work
- Waiting state-style tasks use muted `○`; boxed task rows use muted `□`
- Single-line state transitions: spinner → result (single space before elapsed)
- `✔` success, `✘` failure, `!` warnings, `-` bullets
- Section headings in bold+accent (2-space indent in interactive mode)
- Prompts use diamond style; vanish after the user answers

### Verbose (`--verbose` / `-v`)

Tracing renders — all levels (TRACE through ERROR) with local timestamps and colored level labels. Pretty-only UI functions are no-ops; outcome/error helpers may emit tracing records.

Format: `HH:MM:SS.mmm LEVEL message`

Prompts remain interactive but use transcript style (no screen erasing). Prompts are NOT wrapped in tracing log-level prefixes — they print as plain `eprintln!` text.

Verbose mode must stay transcript-style: print only what is happening now. Do not pre-render upcoming work or persistent task trees here.

### CI (`--ci`)

Same as verbose but without ANSI colors. Prompts use defaults (non-interactive).

CI output is also transcript-style only: emit current work and final results, not upcoming tasks.

### Example: how one operation maps across modes

Normal:
```
◧ Uploading artifact…
✔ Uploaded artifact  711ms
```

Verbose (action completed under 2s — single TRACE end record from `timed()`):
```
10:00:00.200 DEBUG [prod-la] Uploading artifact to /var/tako/releases (12.4 MB)
10:00:00.811 TRACE [prod-la] Upload artifact (711ms)
```

Verbose (action exceeded 2s — deferred DEBUG start + TRACE end):
```
10:00:00.200 DEBUG [prod-la] Uploading artifact to /var/tako/releases (12.4 MB)
10:00:02.200 DEBUG [prod-la] Upload artifact…
10:00:04.300 TRACE [prod-la] Upload artifact (4.1s)
```

CI: identical to verbose but without ANSI colors.

## Interactive Padding

In interactive mode (`is_pretty() && is_interactive()`), plain text output functions
(`info`, `muted`, `hint`, `section`, `heading`) are automatically indented 2 spaces so
they align with symbol-prefixed lines (`✔`/`✘`/`◧` already start at col 0 with
their text at col 2).

Do NOT add manual padding — the output functions handle it.

**Exception: isolated summary blocks.** When a block of text stands on its own with
no spinners or symbol-prefixed lines to align against (e.g. the deploy release +
routes summary), use `output::line(message)` to skip the 2-space indent. The
deploy task tree also renders its success summary flush-left for the same reason.

## Elapsed Times

Pretty output does not use parentheses for elapsed times. The `format_elapsed()` function returns `"3s"`, `"42s"`,
`"1m10s"`. Completion lines use a single space before elapsed:
`✔ Deploy complete 12s`.

When showing size + time: `✔ Downloaded 3s, 72 MB` (comma separator, no parens).

TRACE timing records use `format_elapsed_trace()`, which always returns a value and wraps it in parentheses:
`TRACE SSH connect (250ms)`, `TRACE Upload artifact (4.1s)`.

## Pretty Output API (normal mode only)

Use `output::is_pretty()` to check whether normal pretty UI is active. Pretty-only functions are no-ops in verbose/CI; bridge helpers emit tracing as shown.

### Text Output

| Function | Normal mode | Verbose/CI |
|----------|-------------|------------|
| `section(title)` | blank line + bold accent title (padded) | no-op |
| `heading(title)` | blank line + bold title (padded) | no-op |
| `info(message)` | Default-color text (padded) | no-op |
| `line(message)` | Default-color text (no indent — for isolated summary blocks) | no-op |
| `bullet(message)` | `  - message` | no-op |
| `success(message)` | `✔ message` | `tracing::info!` |
| `success_with_elapsed(message, elapsed)` | `✔ message elapsed` | `tracing::info!` |
| `warning(message)` | `! message` | no-op |
| `error(message)` | Wrapped red error text via `error_block()` | `tracing::error!` |
| `error_block(message)` | Wrapped red error text with no prompt chrome | `tracing::error!` |
| `muted(message)` | Dim text (padded) | no-op |
| `hint(message)` | Dim text (padded) | `tracing::info!` |

### Text Formatting

| Function | Effect | Example |
|----------|--------|---------|
| `strong(value)` | Bold (no color) | App names, server names, key values |
| `accent(value)` | Accent color (no bold) | Secondary emphasis |
| `theme_success(v)` | Green text | Status words: "active", "trusted" |
| `theme_warning(v)` | Amber text | Status words: "disabled", "untrusted" |
| `theme_error(v)` | Red text | Status words: "unreachable", "error" |
| `theme_muted(v)` | Dim text | Elapsed times, metadata |
| `theme_dim(v)` | Dim gray text | Hints and low-emphasis text |

### Environment Context

- **Environment**: only print it when it adds real clarity. Avoid redundant lines like `Using production environment` when the command already shows the environment in the main summary or task tree. Read-only context should use `hint()` or be folded into the main sentence; reserve `warning()` for operations where the selected environment changes something significant. When an environment warning is needed, keep the environment name as plain text inside `warning()`.

## Spinners

Spinners are pure UI: they render animation in pretty mode and are silent in verbose/CI. They do NOT emit tracing. To log an action in verbose/CI, wrap the work in `output::timed()` — it handles both the deferred start (if the action runs >2s) and the end record automatically.

The only exception is errors: spinner helpers that surface a failure still emit `tracing::error!` so failures remain visible in verbose/CI.

**`with_spinner(loading, success, work)`** — Shows spinner if >1s. On success: `✔ success elapsed`.

```rust
let _t = output::timed("Validate config");
output::with_spinner("Validating", "Validated", || {
    validate()?;
    Ok(())
})?;
// Normal: ◧ Validating... → ✔ Validated 1.2s
// Verbose: TRACE Validate config (1.2s)  (single record, action was <2s)
```

**`with_spinner_async(loading, success, work)`** — Same, async.

**`with_spinner_async_err(loading, success, error_label, work)`** — Same as async spinner, but errors render/log with `error_label` instead of `loading`. Use when the in-progress wording is not a good failure label.

**`with_spinner_async_simple(message, work)`** — Async spinner with no result line.

**`with_spinner_silent(message, work)`** — No success output. Errors still render/log. Use for preflight checks where only failures matter.

**`PhaseSpinner::start(message)`** — Major phases (Build, Deploy). Shows elapsed after 1s.

```rust
let _t = output::timed("Build phase");
let phase = output::PhaseSpinner::start("Building…");
// ... build steps ...
phase.finish("Build complete");
// Normal: ◧ Building… 5s → ✔ Build complete 5.2s
// Verbose: DEBUG Build phase…     (deferred start at 2s)
//          TRACE Build phase (5.2s) (end record on drop)
```

**`TrackedSpinner::start(message)`** — Updatable message. `set_message()` is a no-op in verbose/CI.

**`GroupedSpinner::new(parent, children)`** — Parent operation with named sub-steps.
All children start as `·` (pending). Use `start_child()`, `finish_child()`, `fail_child()`.

```rust
let g = output::GroupedSpinner::new("Building services", &["api", "worker"]);
g.start_child("api");
// ... build api ...
g.finish_child("api");
g.start_child("worker");
// ... build worker ...
g.finish_child("worker");
g.finish("Services built");
// Normal:
//   ◧ Building services  10s
//     ✔ api  7s
//     ◧ worker  3s
// Verbose: silent unless caller wraps each step in its own timed() span
```

**`StepFlow::new(steps)`** — Linear phase sequence with pre-rendered pending steps
(pretty mode only; in verbose/CI it is silent — wrap each step in its own `timed()`).

```rust
let flow = output::StepFlow::new(&["Pushing artifact", "Applying migrations", "Health checks"]);
// ... push ...
flow.advance();   // ✔ Pushing artifact  3s, next step activates
// ... migrate ...
flow.advance();   // ✔ Applying migrations  4s
flow.finish();    // ✔ Health checks  2s, spinner cleared
```

## Persistent Task Lists

Use persistent task lists as the preferred pattern for complex interactive flows that already know their plan (`deploy`, `upgrade`, similar multi-step commands).

- Model them as **Tasks** and **Sub tasks**:
  - **Task**: a status-bearing parent row that owns a workflow or a collection of sub tasks.
  - **Sub task**: a single actionable step that may run standalone or inside a task.
- Pretty interactive mode may render the full known task tree up front.
- Waiting leaf rows use a muted pending icon and a trailing `...` label suffix: `○` for state-style rows, `□` for boxed rows.
- Running leaf rows use `◧ ◨ ◩ ◪`; task-tree animation advances at a calm 160ms interval.
- Task rows with sub tasks do not render an icon or elapsed time; running and succeeded parent tasks should keep the accent label. A row becomes a task group by having sub tasks, not by setting a separate no-icon mode.
- Leaf task elapsed time uses a fixed two-space gap after the label/detail; do not right-align elapsed times into a shared column.
- Running sub tasks should keep default text; inline detail segments use a single space separator and should be muted.
- Completed rows stay visible for the life of the command.
- Sub task icons and colors come from the sub task's own state only. Do not rewrite or mute child rows when the parent task finishes. Boxed task rows use `■` for both success and failure; success uses normal text, while failure uses error color and any error detail. Cancelled and skipped sub tasks reuse the muted pending icon (`○`, or `□` for boxed task rows) so they read as work that did not run.
- **Cancelled and skipped rows mute the label too** (not just the icon). Accent color is reserved for rows that are live or finished successfully; cancelled / skipped rows did not complete, so they read as muted across the board, consistent with pending rows.
- Later-discovered conditional work may be appended under the affected parent instead of replacing the original plan.
- Sub task failures may render a related indented error line beneath the sub task. Do not attach that under a task (parent) row.
- If there is only one obvious build task, prefer a single `Building` sub task line over a named section heading.
- When a single build sub task succeeds, change its label to `Built` and keep cache-hit or artifact-size details on child rows instead of the completed parent row.
- For deploy output, render `Connecting to <server>` as a single sub task when there is one target server; with multiple target servers, render a `Connecting` task with one sub task per server. Then render one deploy task per server, for example `Deploying to prod-a` with sub tasks like `Uploading`, `Preparing`, and `Starting`.
- In deploy pretty output, `Connecting` and `Building` should start together once planning is complete. Do not leave `Building` visibly pending if the build task has already been spawned.
- In deploy pretty output, add a blank line after each top-level phase (`Connecting`, `Building`, each `Deploying to ...`) for readability. Do not add blank lines between sub tasks inside a task.
- If a deploy connection check or build step fails, abort the remaining incomplete pretty task-tree rows and mark them as warning `Aborted` instead of leaving them pending.
- Do not keep startup metadata summaries or decorative plan boxes in the live deploy tree when they do not help the operator act.
- Avoid decorative static plan boxes when the task tree already conveys the upcoming work.
- Verbose and CI modes must not show upcoming tasks; they stay transcript-style and only emit current work.
- URLs shown inside summaries or task output must remain literal contiguous `https://...` strings. Do not truncate them, split them across styled segments, or replace them with labels.
- On cancellation, leave exactly one blank line above `Operation cancelled`.

## Transfer Progress

Single-line bar with elapsed time first, then percentage and transferred amount. Completes with `time, size` summary.

```rust
let progress = output::TransferProgress::new("Uploading", "Uploaded", total_bytes);
// Transfer loop:
progress.set_position(bytes_sent);
// On done:
progress.finish();
// Normal: ◧ Uploading…  42s  ████████████░░░░  72%  (84 KB/116 MB) → ✔ Uploaded  42s, 116 MB
```

## Prompts

All prompts work only in interactive mode. In CI mode, they use defaults or error.

**Prompts are NOT log lines.** In verbose mode, prompts print as plain `eprintln!` text — no timestamp, no level prefix.

### Diamond prompt style

Active prompt (pretty mode):
```
◆ App name                ← accent filled diamond + accent label
  › myapp_                ← indented accent chevron on the input line
  hint text here          ← optional muted hint under the input
  enter submit            ← key hint
```

Completed (inactive):
```
◇ App name                ← muted outlined diamond + muted label
  myapp                   ← confirmed value, no chevron
```

Prompt warning, error, and hint lines are indented plain text under the prompt body. Do not add `!` or `✘` chrome inside prompt bodies; the active label/chevron turns red for validation errors.

Validation error example:
```
◆ App name
  › bad_name
  Use lowercase letters, numbers, and hyphens.
  enter submit
```

Confirm prompts keep the default hint on the label line and put descriptions inside the prompt body:
```
◆ Use iCloud Keychain? [y/N]
  Stores the key in your macOS login keychain.
  ›
  enter submit
```

Select prompts use `enter select`, indent options, and render option hints inline in muted parentheses:
```
◆ Source IP mode
  Choose how Tako should find the real client IP.
  → Direct traffic
    PROXY protocol (detected)
  enter select
```

| Function | Normal | Verbose |
|----------|--------|---------|
| `confirm(prompt, default)` | Diamond prompt, vanishing | Plain text transcript |
| `confirm_with_description(prompt, description, default)` | Diamond prompt with indented description | Plain text transcript |
| `TextField::new(label).with_default(default).prompt()` | Diamond prompt, vanishing | Plain text transcript |
| `password_field(prompt)` | Masked `••••••` | Same but masked |
| `select(prompt, items)` | Arrow-key list, diamond summary | Numbered list |
| `TextField::new(label).with_hint(hint).prompt()` | Full builder API | Same |
| `TextField::prompt_validated(validate)` | Shows validation errors under the field | Same |
| `TextField::prompt_validated_with_spinner(validate)` | Animates the prompt marker if validation takes >1s | Same validation, no spinner |
| `Wizard::select_root_back(...)` | Like `Wizard::select`, but ESC/back returns from the first prompt too | Same |
| `Wizard::text_field_named_validated_with_spinner(...)` | Wizard-tracked text field with slow-validation marker | Same |

Use `prompt_validated_with_spinner()` for expensive local validation that should keep the user on the same prompt, such as checking DNS credentials. The spinner replaces the prompt marker while validation is running and does not print a separate success line.

### Error block

```
Invalid value. Try again.
```

`error_block()` prints wrapped red text without borders or prompt chrome. Prompt validation errors are not `error_block()` calls; they render as an indented red line under the active input.

```rust
output::error_block("App name already exists");
```

## Tracing API (verbose/CI mode only)

Use standard tracing macros. They are no-ops in normal mode (no subscriber installed).

```rust
tracing::info!("Uploading artifact");
tracing::debug!("[{name}] Artifact size: {size}");
tracing::trace!("Upload chunk 1/8");
tracing::warn!("Retrying after timeout");
tracing::error!("Upload failed: {err}");
```

### Level guidelines

- **TRACE** — Noisy/repetitive detail, timing spans (`timed()`)
- **DEBUG** — Meaningful internal steps: connections, sizes, paths, versions
- **INFO** — User-visible operation milestones (rarely needed directly — spinners handle this)
- **WARN** — Non-fatal issues
- **ERROR** — Failures

### Message capitalization

Tracing messages that start with a regular word must be capitalized. Messages that start with a name (e.g. `tako-server`, a variable) are fine as-is.

### Scope prefix `[name]`

Use a `[name]` message prefix for per-target context. Do NOT use tracing structured fields.

```rust
tracing::debug!("[{name}] Deploy succeeded");
let ssh_config = SshConfig::from_server(&server.host, server.port).with_label(server_name);
```

### Start/finish records (handled automatically by `timed()`)

Do not manually pair a `tracing::debug!("X…")` with a completion log. `output::timed(label)` enforces the rule for you:

- Always emits a TRACE end record on drop with elapsed time (e.g. `TRACE SSH connect (250ms)`).
- Only emits a DEBUG start record (`DEBUG SSH connect…`) if the action actually exceeds 2 seconds. This is measured, not guessed: a background thread sits on a `Condvar::wait_timeout(2s)`; if the span drops first, `Drop` notifies the condvar and the start log is cancelled; if the 2s elapse first, the start log fires. Fast actions always stay to a single end record; slow actions always advertise themselves at the 2s mark.

Just wrap the work: `let _t = output::timed("SSH connect");`. No need to also write a start log.

### Timing

```rust
let _t = output::timed("SSH connect");
// If action < 2s on drop: TRACE SSH connect (250ms)
// If action ≥ 2s:         DEBUG SSH connect…    (at 2s mark)
//                         TRACE SSH connect (3.4s) (on drop)
```

Fold useful context into the label (host, port, name, size) rather than adding a separate `debug!` next to `timed()`:

```rust
let _t = output::timed(&format!("[{name}] Upload artifact ({} bytes)", size));
```

## Patterns to Follow

### 1. Coexist pretty + tracing

Spinners drive pretty UI, `timed()` drives verbose logs. Don't duplicate start lines.

```rust
output::section("Deploy");

let _t = output::timed(&format!("[{name}] Upload artifact"));
let result = output::with_spinner_async("Uploading", "Uploaded", async {
    tracing::debug!("[{name}] Uploading {size} to {path}");
    upload().await
}).await?;
drop(_t);

output::bullet(&format!("Revision {} deployed", output::strong(rev)));
```

### 2. Single-line state transitions

Every spinner transitions from loading to result:
```
◧ Connecting…        → ✔ Connected
◧ Building… 5s       → ✔ Build complete 5.2s
```

### 3. Phase flow for deploy-style commands

Use `StepFlow` for known sequential phases:
```
◧ Pushing artifact  3s
·  Applying migrations
·  Health checks
```

### 4. Environment warning before destructive commands

```rust
output::warning(&format!("Using {env_name} environment"));
```

Do not nest `accent()` or `strong()` inside `warning()` text. Warning lines own
their color as a whole; nested emphasis creates mixed-color warning rows.

### 5. Accent for emphasis, not quotes

Use `accent()` instead of wrapping in quotes.

### 6. stderr for human output, stdout for data

Human-facing CLI output goes to stderr. Structured data goes to stdout.

## Color Palette

| Name | RGB | Use |
|------|-----|-----|
| ACCENT | `(125, 196, 228)` | Primary CLI color: spinners, section titles, prompt labels |
| THEME_GREEN | `(155, 217, 179)` | `✔`, "active", "trusted", "enabled" |
| THEME_AMBER | `(234, 211, 156)` | `!`, "disabled", "untrusted" |
| THEME_RED | `(232, 163, 160)` | `✘`, "unreachable", "error", error text |
| THEME_CORAL | `(232, 135, 131)` | Dev TUI logo gradient endpoint |
| THEME_TEAL | `(155, 196, 182)` | Dev TUI logo gradient start |

## Anti-Patterns to Avoid

- **Ad-hoc ANSI codes** — Use the output helpers.
- **`println!` for user-facing output** — Use `eprintln!` or output helpers (stderr).
- **Multiple result lines per spinner** — One spinner → one result. Use bullets for details.
- **Spinners for fast operations** — If always <100ms, print result directly.
- **Interactive prompts without CI fallback** — Every prompt must work in `--ci`.
- **Missing `timed()` on remote operations** — Every SSH/network call should have a timing span.
- **DEBUG for noisy repetitive detail** — Use TRACE. Reserve DEBUG for meaningful steps.
- **Sharing formatted messages between modes** — Keep messages plain from the start; never pass ANSI-formatted strings to tracing.
- **Using `strip_ansi` to clean messages** — Don't strip ANSI as a workaround.
- **Manually pairing start + end tracing** — Don't write `tracing::debug!("X…")` next to `timed("X")`. `timed()` handles the deferred start at 2s and the end record automatically.
- **Expecting spinners to emit tracing** — Spinners are silent in verbose/CI (only errors surface). Wrap the underlying action in `timed()` if you want it logged.
- **Tracing structured fields** — Don't use `server = %name` structured fields. Use `[name]` message prefix instead.
- **Wrapping prompts in tracing** — Prompts use `eprintln!` in verbose mode, never `tracing::info!`.
- **Parentheses around pretty elapsed times** — Use `3s` not `(3s)` in pretty output. Use `12s, 72 MB` not `(12s, 72 MB)`. TRACE timing records are the exception and use `(250ms)`.
- **Ad-hoc prompt chrome** — Use the shared diamond prompt style: `◆`/`◇` for the label row, indented `›` on active input rows, plain indented warning/error/hint lines in the prompt body, and no chevron on completed values.
- **Start+finish for fast operations** — Operations under ~2s need only one record. `timed()` enforces this — do not manually emit a start log alongside it.
- **Start messages without `…`** — Every start message that has a corresponding finish must end with `…`. `timed()` adds this automatically on its deferred start.
- **Pre-rendering upcoming steps in verbose/CI** — `·` pending steps only show in pretty mode. `StepFlow` and `GroupedSpinner` handle this automatically.

## Quick Decision Tree

1. **Major phase** (Build, Deploy)? → `section()` + `PhaseSpinner`
2. **Known sequential phases upfront**? → `StepFlow`
3. **Parallel sub-operations with named steps**? → `GroupedSpinner`
4. **Single async operation >500ms**? → `with_spinner_async()`
5. **Single sync operation >500ms**? → `with_spinner()`
6. **File/network transfer with byte count**? → `TransferProgress`
7. **Progress tracking** (N of M)? → `TrackedSpinner`
8. **Result detail** under a phase? → `bullet()`
9. **Validation / inline error**? → `error_block()`
10. **Non-fatal issue**? → `warning()`
11. **Fatal error**? → `error()` then return Err
12. **Meaningful internal step** for debugging? → `tracing::debug!()` with `[scope]` prefix
13. **Noisy/repetitive instrumentation**? → `tracing::trace!()` or `timed()`
14. **Environment context** (read-only)? → `hint()` with plain environment name; destructive context → `warning()`
15. **Low-priority info**? → `muted()`
