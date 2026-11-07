# AgentFlow — Rust Orchestrator (MVP)

Lightweight, local-first Windows CLI to coordinate **Codex CLI (planner/reviewer)** and **Antigravity CLI (executor)** in an isolated Git worktree. **No database server, Docker, web UI, automatic commits or merges.**

> This is an experimental MVP. CI compilation on Windows and actual authenticated Codex/Antigravity runs must pass before production use. Agent prompts and Git worktrees are **not** OS security sandboxes.

## Prerequisites

- Windows 10/11, Rust stable (`rustup`), Git, Codex CLI (`codex`), and Antigravity CLI (`agy`).
- Sign in to both agent CLIs, ensure they run in PowerShell, and check the actual supported arguments of your installed `agy` version.
- Run in a **clean, existing Git repository** with an initial commit; use a disposable copy for first tests.
- Carefully inspect the generated plan; only invoke `approve` after you accept it.

## Build

```powershell
git clone https://github.com/Vo-Xuan-Duong/AgentFlow.git
cd AgentFlow
git switch feat/rust-orchestrator
cargo build --release
.\target\release\agentflow.exe --help
```

## Typical workflow

```powershell
# Run from a separate, clean Git repository that you want to modify:
$af = "C:\path\to\AgentFlow\target\release\agentflow.exe"
& $af doctor
& $af start --request "Implement a small testable feature"
# Copy the task ID printed by the command, inspect plan.md first.
$task = "tREPLACE_WITH_REAL_ID"
& $af approve --task $task
& $af run --task $task
# Populate checks.json with actual non-destructive checks first.
& $af validate --task $task
& $af review --task $task
& $af status --task $task
```

The `--repo C:\path\to\your\repo` option works with every command.

Artifacts are stored **outside** the repository in a sibling `.agentflow-state/<repo>/<task-id>/` directory. Git worktrees are stored in sibling `.agentflow-worktrees/<repo>/<task-id>/`, and the task branch is `agentflow/<task-id>`. This avoids mixing state files with source-code diffs.

Configure `checks.json` in the task artifact directory before validation. Example: `examples/checks-rust.json`. `validate` refuses to pass if no checks have been configured.

## Workflow states

`planning → planned → approved → executing → executed → validating → validated → reviewing → awaiting_human_review`

On any ambiguous agent outcome, the state becomes `needs_attention`. Validation errors become `validation_failed`; all relevant stdout/stderr logs are retained. An operator should inspect errors and the Git worktree; the MVP deliberately avoids automatic retries, commits and merges.

## Security / limitations

- **Manual approval is an explicit CLI step**, not a confirmation dialog; review the plan before running it.
- Git worktrees isolate tracked branches, **not** filesystem access, secrets, credentials or network connectivity.
- No auto-merge, auto-push, force-reset, destructive cleanup, or database migration.
- Do not execute an untrusted repository with unrestricted agent permissions.
- `agy -p` behavior, headless approval prompts and CLI authentication differ across versions; check `agy --help`. A process exit code of zero **does not prove all requested changes were implemented**.
- Initial output validation checks only for changed files, certain permission-related phrases and a stable base commit. Review agent output and the diff manually.
- Codex review is written to `review.md`; it does **not** automatically approve changes. The human decision is the final gate.
- The MVP is sequential, without persistent process sessions, cancellation, automatic code-fix loops or an interactive dashboard.
- This prototype allows user-configured check commands. Only enter commands you understand; run them in a safe repo.

## Windows CI

`.github/workflows/ci.yml` runs Cargo build/lint/test on GitHub Actions Windows runners. Real Codex/Antigravity integration requires the two CLIs installed and authenticated locally.

## Next steps

1. Verify agent CLI flags against current locally installed versions.
2. Add structured agent JSON events and robust permission-denial detection.
3. Add process timeout/cancellation and Windows Job Object-based subprocess termination.
4. Add deterministic review-gate schema and state recovery after interrupted runs.
5. Add `ratatui` TUI and optional multi-task scheduling after the CLI is stable.
