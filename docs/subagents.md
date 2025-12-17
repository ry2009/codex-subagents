# Subagents (experimental)

Codex can run lightweight “subagent” conversations alongside your main session. This is useful for delegating focused work (planning, triage, review, etc.) without polluting the primary chat context.

Subagents are **experimental** and intentionally **resource‑capped**: spawning many agents without limits can make a CLI unresponsive (high CPU / memory) due to local orchestration overhead and tool/process fan‑out.

## Goals

- **Resource-aware**: cap concurrent subagents to keep the UI responsive.
- **Context-efficient**: subagents default to minimal context (no project docs unless explicitly provided).
- **Safe by default**: subagents can be configured to run without mutating tools.
- **Observable**: subagent requests are tagged via `SessionSource::SubAgent(...)` and the `x-openai-subagent` header.

## Enable subagents

Subagents are behind the experimental `subagents` feature flag and are disabled by default.

- Enable in config: add the following to `$CODEX_HOME/config.toml` (usually `~/.codex/config.toml`) and restart Codex:

  ```toml
  [features]
  subagents = true
  ```

- Enable for a single run: launch Codex with `codex --enable subagents`

### Configure budgets (optional)

You can tune subagent resource limits via a `[subagents]` table in `$CODEX_HOME/config.toml` (see also [docs/config.md](./config.md)):

```toml
[subagents]
max_concurrency = 4
max_agents = 128
default_timeout_ms = 1800000
orchestration_timeout_ms = 180000
max_events = 64
max_event_chars = 2048
max_output_chars = 32768
```

## How it works (high level)

When enabled, Codex exposes subagent tools to the model:

- `delegate`: synchronous one-shot delegation (returns the subagent output directly).
- `subagent_spawn` / `subagent_poll`: spawn a background one-shot subagent and check in on it.
- `subagent_cancel`: cancel a running subagent.
- `subagent_list`: list subagents spawned in the current session.
- `subagent_resume`: resume a previous rollout file as initial history and run a new prompt.

All subagent requests are tagged via `SessionSource::SubAgent(...)` and sent with the `x-openai-subagent` header.

## TUI: manage subagents

In the Codex TUI you can inspect and manage background subagents without involving the model:

- Run `/subagents` to list known subagents in the current session.
- Select a subagent to open an action menu:
  - **Poll**: fetch its latest status/output.
  - **Cancel**: request cancellation.

Poll output is printed into the main transcript as an info block.

### TUI: multi-agent commands

When the `subagents` feature flag is enabled, Codex also exposes two convenience commands built on background subagents:

- `/plan <task>`: spawns multiple planning subagents and prints a consolidated plan.
- `/solve <task>`: spawns multiple solving subagents and prints a consolidated recommendation.

Orchestration subagents run in `explore` mode and disable tool-heavy features (`shell`, `unified_exec`, `apply_patch`, `web_search`, `view_image`) so they can run unattended without blocking on approvals.

These orchestration helpers are time-bounded by `[subagents].orchestration_timeout_ms` (and will cancel stragglers).

### CLI: multi-agent commands (non-interactive)

If you don't want to run the interactive TUI, you can also invoke the same orchestration workflows as CLI subcommands:

- `codex plan "<task>"`
- `codex solve "<task>"`

These commands print a warning with spawned agent ids and then print the consolidated Markdown report.

## `delegate` (synchronous one-shot)

Arguments:

- `prompt` (required): the subagent prompt.
- `label` (optional): telemetry tag (sent as `x-openai-subagent`).
- `skills` (optional): list of skill names to inject.
- `allow_tools` (optional): opt into tool access (defaults to false).
- `timeout_ms` (optional): deadline for the subagent run.

By default, `delegate` uses `[subagents].orchestration_timeout_ms` as its timeout and truncates output to `[subagents].max_output_chars`.

## Background subagents (`subagent_spawn` / `subagent_poll`)

Background subagents are designed for longer work: you can spawn one, keep chatting, and poll later (or poll with `await_ms` to block until it finishes).

### `subagent_spawn`

Arguments:

- `prompt` (required): the subagent prompt.
- `label` (optional): telemetry tag (sent as `x-openai-subagent`).
- `mode` (optional): subagent profile (`general` (default) or `explore`).
- `skills` (optional): list of skill names to inject.
- `timeout_ms` (optional): deadline for the subagent run (defaults to 30 minutes).
- `agent_id` (optional): explicit agent id (useful for deterministic orchestration/tests).

Returns a JSON blob containing `agent_id`, `status` (`queued`), `label`, and `mode`.

### `subagent_poll`

Arguments:

- `agent_id` (required): id from `subagent_spawn`.
- `await_ms` (optional): time to wait for progress before returning (useful to “check in” without tight polling loops).

Returns a JSON blob including `status` (`queued` | `running` | `complete` | `aborted` | `error`) and `final_output` when complete.

### Approvals

Background subagents can request approvals (exec / apply_patch). These approval prompts are surfaced to the parent session, and decisions are forwarded back to the subagent.

## `subagent_resume`

`subagent_resume` is the “resumable subagent” primitive. It seeds a new subagent run with an existing rollout file and then runs a new prompt.

Arguments:

- `rollout_path` (required): path to a Codex rollout `.jsonl` file.
- `prompt` (required): the new prompt to run.
- `label` / `mode` / `skills` / `timeout_ms` / `agent_id` (optional): same meaning as `subagent_spawn`.

## Performance notes

To avoid “subagents melt my laptop” scenarios, Codex:

- Limits the number of concurrent subagent runs (see `[subagents].max_concurrency`).
- Avoids copying full conversation state into subagents by default.
- Disables subagent recursion (a subagent cannot spawn more subagents).
- Budgets per-subagent retained output/event sizes (see `[subagents]`).

## Benchmarking

There is an ignored, benchmark-style integration test you can run locally:

```bash
cd codex-rs
cargo test -p codex-core bench_subagents_spawn_poll_16 -- --ignored --nocapture
```
