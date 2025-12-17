use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;

use crate::function_tool::FunctionCallError;
use crate::subagents::SubagentMode;
use crate::subagents::SubagentSpawnRequest;
use crate::subagents::SubagentStatus;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

pub struct SubagentHandler;

const DEFAULT_SUBAGENT_LABEL: &str = "subagent";
const MAX_LABEL_LEN: usize = 48;

#[derive(Debug, Deserialize)]
struct SubagentSpawnArgs {
    #[serde(default)]
    agent_id: Option<String>,
    prompt: String,
    #[serde(default)]
    label: Option<String>,
    /// Built-in profile name ("general" or "explore").
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct SubagentResumeArgs {
    #[serde(default)]
    agent_id: Option<String>,
    rollout_path: String,
    prompt: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct SubagentIdArgs {
    agent_id: String,
}

#[derive(Debug, Deserialize)]
struct SubagentPollArgs {
    agent_id: String,
    /// Optional time to wait for status changes (milliseconds).
    #[serde(default)]
    await_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct SpawnResponse {
    agent_id: String,
    status: String,
    label: String,
    mode: String,
}

#[derive(Debug, Serialize)]
struct PollResponse {
    agent_id: String,
    status: String,
    label: String,
    mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    rollout_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_output: Option<String>,
    recent_events: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ListResponse {
    agents: Vec<PollResponse>,
}

fn sanitize_label(label: &str) -> String {
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

fn status_str(status: SubagentStatus) -> &'static str {
    match status {
        SubagentStatus::Queued => "queued",
        SubagentStatus::Running => "running",
        SubagentStatus::Complete => "complete",
        SubagentStatus::Aborted => "aborted",
        SubagentStatus::Error => "error",
    }
}

fn mode_from_args(mode: Option<String>) -> Result<SubagentMode, String> {
    let mode = mode.unwrap_or_else(|| "general".to_string());
    SubagentMode::from_str(&mode)
        .ok_or_else(|| "unknown subagent mode; expected one of: general, explore".to_string())
}

fn cap_output(text: Option<String>, max_output_chars: usize) -> Option<String> {
    let mut text = text?;
    if text.len() > max_output_chars {
        text = codex_utils_string::take_bytes_at_char_boundary(&text, max_output_chars).to_string();
    }
    Some(text)
}

#[async_trait]
impl ToolHandler for SubagentHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            tool_name,
            payload,
            ..
        } = invocation;

        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "subagent tools expect a function payload".to_string(),
            ));
        };

        match tool_name.as_str() {
            "subagent_spawn" => {
                let args: SubagentSpawnArgs = serde_json::from_str(&arguments).map_err(|e| {
                    FunctionCallError::RespondToModel(format!(
                        "failed to parse function arguments: {e:?}"
                    ))
                })?;
                let prompt = args.prompt.trim();
                if prompt.is_empty() {
                    return Err(FunctionCallError::RespondToModel(
                        "subagent_spawn.prompt must be non-empty".to_string(),
                    ));
                }

                let mode = mode_from_args(args.mode).map_err(FunctionCallError::RespondToModel)?;
                let label = sanitize_label(args.label.as_deref().unwrap_or(DEFAULT_SUBAGENT_LABEL));

                let parent_config = turn.client.config().as_ref().clone();
                let resp = session
                    .services
                    .subagent_manager
                    .spawn_one_shot(
                        SubagentSpawnRequest {
                            agent_id: args.agent_id,
                            mode,
                            label: label.clone(),
                            prompt: prompt.to_string(),
                            skills: args.skills,
                            timeout_ms: args.timeout_ms,
                            resume_rollout_path: None,
                        },
                        session.clone(),
                        turn.clone(),
                        session.services.auth_manager.clone(),
                        session.services.models_manager.clone(),
                        session.services.skills_manager.clone(),
                        parent_config,
                    )
                    .await;

                let resp = resp.map_err(FunctionCallError::RespondToModel)?;
                let out = SpawnResponse {
                    agent_id: resp.agent_id,
                    status: status_str(resp.status).to_string(),
                    label: resp.label,
                    mode: resp.mode.as_str().to_string(),
                };
                Ok(ToolOutput::Function {
                    content: serde_json::to_string(&out)
                        .unwrap_or_else(|_| "{\"error\":\"failed to serialize\"}".to_string()),
                    content_items: None,
                    success: Some(true),
                })
            }
            "subagent_resume" => {
                let args: SubagentResumeArgs = serde_json::from_str(&arguments).map_err(|e| {
                    FunctionCallError::RespondToModel(format!(
                        "failed to parse function arguments: {e:?}"
                    ))
                })?;
                let prompt = args.prompt.trim();
                if prompt.is_empty() {
                    return Err(FunctionCallError::RespondToModel(
                        "subagent_resume.prompt must be non-empty".to_string(),
                    ));
                }

                let rollout_path = args.rollout_path.trim();
                if rollout_path.is_empty() {
                    return Err(FunctionCallError::RespondToModel(
                        "subagent_resume.rollout_path must be non-empty".to_string(),
                    ));
                }

                let mode = mode_from_args(args.mode).map_err(FunctionCallError::RespondToModel)?;
                let label = sanitize_label(args.label.as_deref().unwrap_or(DEFAULT_SUBAGENT_LABEL));
                let parent_config = turn.client.config().as_ref().clone();
                let resp = session
                    .services
                    .subagent_manager
                    .spawn_one_shot(
                        SubagentSpawnRequest {
                            agent_id: args.agent_id,
                            mode,
                            label: label.clone(),
                            prompt: prompt.to_string(),
                            skills: args.skills,
                            timeout_ms: args.timeout_ms,
                            resume_rollout_path: Some(std::path::PathBuf::from(rollout_path)),
                        },
                        session.clone(),
                        turn.clone(),
                        session.services.auth_manager.clone(),
                        session.services.models_manager.clone(),
                        session.services.skills_manager.clone(),
                        parent_config,
                    )
                    .await;

                let resp = resp.map_err(FunctionCallError::RespondToModel)?;
                let out = SpawnResponse {
                    agent_id: resp.agent_id,
                    status: status_str(resp.status).to_string(),
                    label: resp.label,
                    mode: resp.mode.as_str().to_string(),
                };
                Ok(ToolOutput::Function {
                    content: serde_json::to_string(&out)
                        .unwrap_or_else(|_| "{\"error\":\"failed to serialize\"}".to_string()),
                    content_items: None,
                    success: Some(true),
                })
            }
            "subagent_poll" => {
                let args: SubagentPollArgs = serde_json::from_str(&arguments).map_err(|e| {
                    FunctionCallError::RespondToModel(format!(
                        "failed to parse function arguments: {e:?}"
                    ))
                })?;
                let Some(poll) = session
                    .services
                    .subagent_manager
                    .poll(&args.agent_id, args.await_ms)
                    .await
                else {
                    return Err(FunctionCallError::RespondToModel(
                        "unknown agent_id".to_string(),
                    ));
                };

                let max_output_chars = turn.client.config().subagents.max_output_chars;
                let out = PollResponse {
                    agent_id: poll.agent_id,
                    status: status_str(poll.status).to_string(),
                    label: poll.label,
                    mode: poll.mode.as_str().to_string(),
                    rollout_path: poll.rollout_path.as_ref().map(|p| p.display().to_string()),
                    final_output: cap_output(poll.final_output, max_output_chars),
                    recent_events: poll.recent_events,
                };
                Ok(ToolOutput::Function {
                    content: serde_json::to_string(&out)
                        .unwrap_or_else(|_| "{\"error\":\"failed to serialize\"}".to_string()),
                    content_items: None,
                    success: Some(true),
                })
            }
            "subagent_cancel" => {
                let args: SubagentIdArgs = serde_json::from_str(&arguments).map_err(|e| {
                    FunctionCallError::RespondToModel(format!(
                        "failed to parse function arguments: {e:?}"
                    ))
                })?;
                if session
                    .services
                    .subagent_manager
                    .cancel(&args.agent_id)
                    .await
                    .is_none()
                {
                    return Err(FunctionCallError::RespondToModel(
                        "unknown agent_id".to_string(),
                    ));
                }
                Ok(ToolOutput::Function {
                    content: "{\"status\":\"cancelled\"}".to_string(),
                    content_items: None,
                    success: Some(true),
                })
            }
            "subagent_list" => {
                let agents = session.services.subagent_manager.list().await;
                let max_output_chars = turn.client.config().subagents.max_output_chars;
                let out = ListResponse {
                    agents: agents
                        .into_iter()
                        .map(|poll| PollResponse {
                            agent_id: poll.agent_id,
                            status: status_str(poll.status).to_string(),
                            label: poll.label,
                            mode: poll.mode.as_str().to_string(),
                            rollout_path: poll
                                .rollout_path
                                .as_ref()
                                .map(|p| p.display().to_string()),
                            final_output: cap_output(poll.final_output, max_output_chars),
                            recent_events: poll.recent_events,
                        })
                        .collect(),
                };
                Ok(ToolOutput::Function {
                    content: serde_json::to_string(&out)
                        .unwrap_or_else(|_| "{\"error\":\"failed to serialize\"}".to_string()),
                    content_items: None,
                    success: Some(true),
                })
            }
            _ => Err(FunctionCallError::Fatal(format!(
                "unknown subagent tool: {tool_name}"
            ))),
        }
    }
}
