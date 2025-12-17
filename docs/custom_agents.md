# Custom agents (experimental)

Custom agents are named prompt templates that run as background subagents. They’re useful for saving “roles” (planner, reviewer, scout, etc.) and invoking them on demand without polluting the main chat context.

Custom agents require the experimental `subagents` feature flag. See [docs/subagents.md](./subagents.md) for enablement and subagent budgeting.

## Define an agent

Create a Markdown file in one of these locations:

- Repo scope (checked in): `.codex/agents/<name>.md`
- User scope (per-machine): `$CODEX_HOME/agents/<name>.md` (usually `~/.codex/agents/<name>.md`)

If an agent name exists in both places, the repo-scoped file wins.

### File format

Files are Markdown with optional YAML frontmatter:

```md
---
name: reviewer
description: Review changes for bugs and missing tests
model: gpt-5.1-codex
mode: explore # explore|general
tools:
  - read_file
  - list_dir
  - grep_files
---

You are a careful code reviewer. Focus on correctness and test coverage.
Prefer small, reviewable changes.
```

Supported frontmatter fields:

- `name` (optional): defaults to the filename stem; normalized to lowercase `a-z0-9-_`.
- `description` / `role` (optional): shown in `/agents`.
- `model` (optional): defaults to the current session model.
- `mode` (optional): `explore` (planning/review) or `general` (full workflow, subject to approvals).
- `tools` (optional):
  - `inherit` / `true`: use the parent session’s tools.
  - `none` / `false`: disable all tools.
  - list: restrict tools to an allowlist (tool names are matched case-insensitively).

The Markdown body becomes the agent’s prompt (injected into developer instructions for the subagent run).

## Use an agent

In the TUI:

- `/agents` to list available agents
- `/agent <name> <task>` to run an agent (spawns a background subagent and waits by default)
- `/subagents` to poll/cancel background runs

In the CLI:

- `codex agents`
- `codex agent <name> "<task>" [--no-wait] [--timeout-ms <ms>]`

## Notes

- A subagent can’t spawn other subagents (no recursion).
- Use `tools: none` or an allowlist for unattended agents to avoid approval deadlocks.
