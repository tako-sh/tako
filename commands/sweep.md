---
description: Security, performance, overengineering, and code quality audit
---

# Sweep

Combined security, performance, overengineering, and code quality audit. Find and fix real problems only; if there is nothing high-confidence to report, that is a valid outcome. Commit auto-fixes along the way.

## Phase 1 — Look for real issues

Analyze the codebase for real, evidence-backed issues across these categories:

- Security (injection, auth, secrets, supply chain, trust boundaries)
- Performance (hot paths, blocking I/O, unnecessary allocations, algorithmic complexity)
- Code quality (dead code, redundant logic, error handling gaps, inconsistencies, unnecessary complexity)
- Overengineering / unnecessary complexity (speculative guardrails, unused abstractions, redundant validation, premature configurability, future-proofing for cases the product does not need)
- Configuration & operational (misconfigs, missing limits, unsafe defaults)

**Method:**

1. Map trust boundaries and execution paths first.
2. Separate real boundaries from trusted internal paths. Do not demand guards inside trusted internal code unless the value crosses a user, network, filesystem, process, or third-party boundary there.
3. Focus on realistic exploitability, real bottlenecks, and real maintenance cost — skip theoretical, stylistic, and "just in case" nits.
4. Every issue must have concrete evidence (file:line) and a reason it matters in normal use or a plausible production failure.
5. Do not force a finding to justify the sweep. If the evidence does not support a current issue, leave it out.
6. If tools/profilers can't run, say so and continue with static analysis.

## Phase 2 — Fix what's obvious

For each issue found, decide:

- **Auto-fix**: If the fix is trivial, safe, clearly correct, and doesn't require architectural decisions or my input — fix it immediately. Prefer deleting, simplifying, or moving checks to the real boundary over adding new code. Examples: dead code removal, removing unused abstraction layers, deleting unreachable fallback paths, collapsing redundant validation, obvious resource leaks, missing validation at a real external boundary.
- **Skip**: If the fix requires my input, is risky, or involves tradeoffs — don't touch it, carry it to Phase 3.

Auto-fixes must not add speculative guardrails, new configuration knobs, new abstractions, new retry/fallback paths, or validation for states that cannot occur in the current architecture. Do not refactor, tidy, or "improve" code just because it is nearby. If a proposed fix starts with "in case someday," "while I'm here," or "this might be useful later," skip it.

Commit each batch of auto-fixes as you go (use the current branch). Group related fixes into logical commits with conventional commit messages.

After fixing, run a second audit pass to catch regressions or newly exposed issues.

## Phase 3 — Report remaining issues

Present **only P0/P1 issues and high-confidence unnecessary-complexity cleanup candidates** that remain after auto-fixes. Unnecessary complexity belongs here only when it has concrete current cost, such as confusing control flow, unused public surface, duplicate configuration paths, brittle tests, or extra code that makes normal changes harder. Do not include optional refactors, style cleanup, broad modernization, or improvements that are merely nice to have. Use this exact format:

```
### Remaining Issues

1. **[Security|Perf|Quality] Short title** — `file:line`
   What: one-line description of the problem
   Risk: what happens if not fixed
   Recommendation: what to do (if there are options, list them as A/B/C so I can pick)
   Input needed: what decision I need to make (or "none — just needs implementation time")

2. ...
```

Keep it flat — no nested sections, no executive summaries, no tables. Just the numbered list.

If auto-fixes were made in Phase 2, list them briefly at the top:

```
### Auto-fixed
- Short description of fix (`file:line`)
- ...
```

If no P0/P1 issues or high-confidence unnecessary-complexity cleanup candidates remain:

> **No high-priority issues found.**

## Rules

- Evidence only — no speculative warnings.
- No forced outcomes. A sweep can legitimately end with no auto-fixes and no remaining issues.
- Flag unnecessary complexity when it has concrete maintenance cost: abstractions with a single implementation, indirection that doesn't pay for itself, configurability nobody uses, redundant cross-layer validation, error handling for impossible states, fallback paths that cannot be reached, or solving problems that aren't real yet. Simple code that does the job beats clever code that anticipates hypotheticals.
- When fixing unnecessary complexity, remove or simplify the unnecessary mechanism. Do not replace one speculative mechanism with another.
- Do not refactor for taste, neatness, modernization, or broad cleanup. You are not here to create work; you are here to remove current risk or current drag.
- Only add a guardrail when the current code crosses a real boundary or has an observed production failure mode. Internal calls between trusted components should rely on their contract.
- Fewer high-confidence findings over many weak ones.
- Don't report best practices unless the code demonstrably violates them in a way that matters.
- Suspicious-but-unprovable items go in a short "Blind spots" list at the end, not in findings.
- Ignore issues in code paths that exist only for development or testing (e.g. `#[cfg(test)]` modules, test helper commands, dev-only socket commands). Focus on code that runs in production builds.
- Do not add tests that assert removed symbols, old flags, legacy messages, or deleted behaviors are absent. Tests should cover current behavior and real distinctions only.

## Do not report

Before adding an item to the report, check this list. If any apply, drop it — do not promote it to Phase 3.

- **"Not exploitable in practice"** — if you have to write this in the Risk line, the finding doesn't belong in the report. Move it to Blind spots or delete it.
- **Textbook hardening with no concrete attacker model** — e.g., non-constant-time comparison of a high-entropy env-scoped token over a network, timing side channels behind JIT/GC noise, redundant length checks where the type system already guarantees bounds. Only report a timing/hardening issue if you can describe a concrete attacker and realistic exploit path.
- **Defense-in-depth suggestions for a layer that already sits behind a trusted gate** — if a separate component (proxy, firewall, auth middleware) enforces the real boundary, don't ask the inner layer to duplicate the check "just in case." Trust the architecture; flag the outer gate if it's weak.
- **Cross-layer validation where the project's convention says otherwise** — if security guards live in Rust (or Go, or a specific service) by design, don't report JS/TS/peripheral code for not re-validating. Respect the trust boundary the codebase has chosen.
- **Code-simplification suggestions dressed as security** — "this check is redundant with X" or "this fallback is unnecessary" is a refactor, not a finding. Skip.
- **Guardrails for impossible states** — if the type system, parser, protocol, or construction path makes the state impossible, don't add runtime checks. If the proof is unclear, explain the uncertainty in Blind spots instead of fixing it.
- **Future-proofing** — don't add extension points, options, fallback modes, retries, or compatibility paths for requirements that do not exist today.
- **Widening-not-tightening "fixes"** — adding entries to an allow-list (e.g., extra loopback hosts), adding more accepted inputs, or making a gate more permissive is never a security fix. Skip.
- **Findings where the recommendation is "document better"** — docs gaps are not P0/P1 security or perf issues.
