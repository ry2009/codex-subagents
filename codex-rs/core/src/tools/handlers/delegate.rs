use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use codex_protocol::user_input::UserInput;
use serde::Deserialize;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::codex_delegate::run_codex_conversation_one_shot;
use crate::features::Feature;
use crate::function_tool::FunctionCallError;
use crate::protocol::EventMsg;
use crate::protocol::SandboxPolicy;
use crate::protocol::SubAgentSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

pub struct DelegateHandler;

const DEFAULT_SUBAGENT_LABEL: &str = "delegate";
const MAX_LABEL_LEN: usize = 48;

#[derive(Debug, Deserialize)]
struct DelegateArgs {
    prompt: String,

    #[serde(default)]
    label: Option<String>,

    /// Skill names to inject into the subagent as `UserInput::Skill`.
    #[serde(default)]
    skills: Vec<String>,

    /// If false (default), the subagent is configured with tool features disabled.
    #[serde(default)]
    allow_tools: bool,

    /// Optional deadline for the subagent run.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

fn sanitize_subagent_label(label: &str) -> String {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return DEFAULT_SUBAGENT_LABEL.to_string();
    }

    let mut out = String::new();
    for ch in trimmed.chars() {
        if out.len() >= MAX_LABEL_LEN {
            break;
        }
        match ch {
            'a'..='z' | '0'..='9' | '-' | '_' | '.' => out.push(ch),
            'A'..='Z' => out.push(ch.to_ascii_lowercase()),
            ' ' | '/' | ':' => out.push('-'),
            _ => {}
        }
    }

    if out.is_empty() {
        DEFAULT_SUBAGENT_LABEL.to_string()
    } else {
        out
    }
}

fn delegate_base_instructions(label: &str, allow_tools: bool) -> String {
    let tools_line = if allow_tools {
        "- Tools: You may call tools if needed, but prefer minimal, read-only actions.\n"
    } else {
        "- Tools: Do not call tools. If you need data, request specific files/commands from the parent.\n"
    };

    format!(
        "You are a focused subagent named \"{label}\".\n\
Your job is to help the parent Codex session by producing a concise, actionable result.\n\
\n\
Requirements:\n\
- Output: respond with only your final answer (no meta commentary).\n\
- Scope: focus only on the delegated prompt.\n\
{tools_line}\
- Efficiency: keep the response short; prefer checklists and concrete next steps.\n"
    )
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[async_trait]
impl ToolHandler for DelegateHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            ..
        } = invocation;

        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "delegate expects a function payload".to_string(),
            ));
        };

        let args: DelegateArgs = serde_json::from_str(&arguments).map_err(|e| {
            FunctionCallError::RespondToModel(format!("failed to parse function arguments: {e:?}"))
        })?;

        let prompt = args.prompt.trim();
        if prompt.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "delegate.prompt must be non-empty".to_string(),
            ));
        }

        let label =
            sanitize_subagent_label(args.label.as_deref().unwrap_or(DEFAULT_SUBAGENT_LABEL));

        let _permit = crate::subagents::global_subagent_limiter()
            .acquire_owned()
            .await
            .map_err(|_| {
                FunctionCallError::Fatal(
                    "delegate concurrency limiter closed unexpectedly".to_string(),
                )
            })?;

        let timeout_duration = args
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(turn.client.config().subagents.orchestration_timeout);
        let max_output_chars = turn.client.config().subagents.max_output_chars;

        let mut sub_agent_config = turn.client.config().as_ref().clone();

        // Prevent recursion: subagents should not be able to spawn more subagents.
        sub_agent_config.features.disable(Feature::Subagents);
        // Avoid background git/process churn in ephemeral delegate sessions.
        sub_agent_config.features.disable(Feature::GhostCommit);

        // By default we keep subagents lightweight: skip project docs and the parent AGENTS.md.
        sub_agent_config.user_instructions = None;
        sub_agent_config.developer_instructions =
            Some(delegate_base_instructions(&label, args.allow_tools));
        sub_agent_config.project_doc_max_bytes = 0;

        // Default to a safe sandbox even when tools are enabled (if the user opts in).
        sub_agent_config.sandbox_policy = SandboxPolicy::new_read_only_policy();

        if !args.allow_tools {
            sub_agent_config
                .features
                .disable(Feature::ShellTool)
                .disable(Feature::UnifiedExec)
                .disable(Feature::ApplyPatchFreeform)
                .disable(Feature::WebSearchRequest)
                .disable(Feature::ViewImageTool)
                .disable(Feature::ShellSnapshot);
        }

        if !args.skills.is_empty() {
            sub_agent_config.features.enable(Feature::Skills);
        }

        let mut inputs: Vec<UserInput> = Vec::new();
        inputs.push(UserInput::Text {
            text: prompt.to_string(),
        });

        if !args.skills.is_empty() {
            let outcome = session.services.skills_manager.skills_for_cwd(&turn.cwd);
            let mut missing: Vec<String> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();

            for name in args.skills {
                if !seen.insert(name.clone()) {
                    continue;
                }
                if let Some(skill) = outcome.skills.iter().find(|s| s.name == name) {
                    inputs.push(UserInput::Skill {
                        name: skill.name.clone(),
                        path: skill.path.clone(),
                    });
                } else {
                    missing.push(name);
                }
            }

            if !missing.is_empty() {
                return Err(FunctionCallError::RespondToModel(format!(
                    "unknown skills requested: {}; check the available skills list",
                    missing.join(", ")
                )));
            }
        }

        let cancel_token = CancellationToken::new();
        let _cancel_on_drop = CancelOnDrop(cancel_token.clone());

        let subagent = run_codex_conversation_one_shot(
            sub_agent_config,
            Arc::clone(&session.services.auth_manager),
            Arc::clone(&session.services.models_manager),
            inputs,
            Arc::clone(&session),
            Arc::clone(&turn),
            cancel_token.clone(),
            None,
            SubAgentSource::Other(label),
        )
        .await
        .map_err(|e| FunctionCallError::RespondToModel(format!("delegate failed to start: {e}")))?;

        let output = timeout(timeout_duration, async {
            let mut last_error: Option<String> = None;
            loop {
                let event = subagent.next_event().await.map_err(|e| {
                    FunctionCallError::RespondToModel(format!(
                        "delegate subagent failed while waiting for output: {e}"
                    ))
                })?;
                match event.msg {
                    EventMsg::Error(ev) => {
                        last_error = Some(ev.message);
                    }
                    EventMsg::StreamError(ev) => {
                        last_error = Some(ev.message);
                    }
                    EventMsg::TaskComplete(task_complete) => {
                        let Some(text) = task_complete.last_agent_message else {
                            if let Some(err) = last_error {
                                return Err(FunctionCallError::RespondToModel(format!(
                                    "delegate subagent failed: {err}"
                                )));
                            }
                            return Err(FunctionCallError::RespondToModel(
                                "delegate subagent produced no final output".to_string(),
                            ));
                        };
                        return Ok(text);
                    }
                    EventMsg::TurnAborted(_) => {
                        return Err(FunctionCallError::RespondToModel(
                            "delegate subagent was aborted".to_string(),
                        ));
                    }
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| {
            cancel_token.cancel();
            FunctionCallError::RespondToModel(format!(
                "delegate timed out after {}ms",
                timeout_duration.as_millis()
            ))
        })??;

        Ok(ToolOutput::Function {
            content: codex_utils_string::take_bytes_at_char_boundary(&output, max_output_chars)
                .to_string(),
            content_items: None,
            success: Some(true),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn sanitize_label_defaults_and_truncates() {
        assert_eq!(sanitize_subagent_label(""), DEFAULT_SUBAGENT_LABEL);
        assert_eq!(sanitize_subagent_label("   "), DEFAULT_SUBAGENT_LABEL);
        assert_eq!(sanitize_subagent_label("My Agent"), "my-agent");
        assert_eq!(sanitize_subagent_label("a/b:c"), "a-b-c");
        assert_eq!(sanitize_subagent_label("😅"), DEFAULT_SUBAGENT_LABEL);
        assert_eq!(
            sanitize_subagent_label(&"a".repeat(MAX_LABEL_LEN + 10)),
            "a".repeat(MAX_LABEL_LEN)
        );
    }
}
