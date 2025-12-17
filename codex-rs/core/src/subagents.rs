use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use codex_protocol::protocol::ApplyPatchApprovalRequestEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecApprovalRequestEvent;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::RwLock;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::AuthManager;
use crate::codex::Codex;
use crate::codex::CodexSpawnOk;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::features::Feature;
use crate::openai_models::models_manager::ModelsManager;
use crate::protocol::AskForApproval;
use crate::protocol::SandboxPolicy;
use crate::rollout::RolloutRecorder;
use crate::skills::SkillsManager;

const SESSION_CONFIGURED_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_AGENT_ID_LEN: usize = 64;

static SUBAGENT_CONCURRENCY_LIMITER: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn default_max_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(2)
        .clamp(1, 4)
}

pub(crate) fn init_global_subagent_limiter(max_concurrency: Option<usize>) {
    if SUBAGENT_CONCURRENCY_LIMITER.get().is_some() {
        return;
    }

    let max_concurrency = max_concurrency
        .unwrap_or_else(default_max_concurrency)
        .clamp(1, 64);
    let _ = SUBAGENT_CONCURRENCY_LIMITER.set(Arc::new(Semaphore::new(max_concurrency)));
}

pub(crate) fn global_subagent_limiter() -> Arc<Semaphore> {
    SUBAGENT_CONCURRENCY_LIMITER
        .get_or_init(|| Arc::new(Semaphore::new(default_max_concurrency())))
        .clone()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubagentMode {
    /// Read-only, tool-light profile meant for exploration and planning.
    Explore,
    /// General-purpose profile that can edit and run tools (subject to approvals).
    General,
}

impl SubagentMode {
    pub(crate) fn from_str(mode: &str) -> Option<Self> {
        match mode.trim().to_ascii_lowercase().as_str() {
            "explore" | "explorer" | "read-only" | "readonly" => Some(Self::Explore),
            "general" | "default" | "worker" => Some(Self::General),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Explore => "explore",
            Self::General => "general",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SubagentSpawnRequest {
    pub(crate) agent_id: Option<String>,
    pub(crate) mode: SubagentMode,
    pub(crate) label: String,
    pub(crate) prompt: String,
    pub(crate) skills: Vec<String>,
    pub(crate) timeout_ms: Option<u64>,
    pub(crate) resume_rollout_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub(crate) struct SubagentSpawnResponse {
    pub(crate) agent_id: String,
    pub(crate) status: SubagentStatus,
    pub(crate) label: String,
    pub(crate) mode: SubagentMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubagentStatus {
    Queued,
    Running,
    Complete,
    Aborted,
    Error,
}

impl Default for SubagentStatus {
    fn default() -> Self {
        Self::Queued
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SubagentPollResponse {
    pub(crate) agent_id: String,
    pub(crate) status: SubagentStatus,
    pub(crate) label: String,
    pub(crate) mode: SubagentMode,
    pub(crate) rollout_path: Option<PathBuf>,
    pub(crate) final_output: Option<String>,
    pub(crate) recent_events: Vec<String>,
}

#[derive(Default)]
struct SubagentState {
    status: SubagentStatus,
    rollout_path: Option<PathBuf>,
    final_output: Option<String>,
    recent_events: VecDeque<String>,
    last_update: Option<Instant>,
}

struct SubagentHandle {
    id: String,
    label: String,
    mode: SubagentMode,
    cancel: CancellationToken,
    notify: Notify,
    state: Mutex<SubagentState>,
    created_at: Instant,
    max_events: usize,
    max_event_chars: usize,
    max_output_chars: usize,
}

#[derive(Default)]
pub(crate) struct SubagentManager {
    agents: RwLock<HashMap<String, Arc<SubagentHandle>>>,
}

fn sanitize_agent_id(agent_id: &str) -> Option<String> {
    let trimmed = agent_id.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut out = String::new();
    for ch in trimmed.chars() {
        if out.len() >= MAX_AGENT_ID_LEN {
            break;
        }
        match ch {
            'a'..='z' | '0'..='9' | '-' | '_' => out.push(ch),
            'A'..='Z' => out.push(ch.to_ascii_lowercase()),
            _ => {}
        }
    }

    if out.is_empty() { None } else { Some(out) }
}

impl SubagentManager {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn spawn_one_shot(
        &self,
        req: SubagentSpawnRequest,
        parent_session: Arc<Session>,
        parent_turn: Arc<TurnContext>,
        auth_manager: Arc<AuthManager>,
        models_manager: Arc<ModelsManager>,
        skills_manager: Arc<SkillsManager>,
        parent_config: crate::config::Config,
    ) -> Result<SubagentSpawnResponse, String> {
        let max_agents = parent_config.subagents.max_agents;
        let mut prune_candidates: Vec<(Instant, String)> = Vec::new();

        let label = req.label.clone();
        let mode = req.mode;
        let agent_id = if let Some(requested) = req.agent_id.as_deref() {
            sanitize_agent_id(requested).ok_or_else(|| "invalid agent_id".to_string())?
        } else {
            Uuid::new_v4().to_string()
        };

        if max_agents == 0 {
            return Err("subagents.max_agents must be >= 1".to_string());
        }

        {
            let agents = self.agents.read().await;
            if agents.contains_key(&agent_id) {
                return Err("agent_id already exists".to_string());
            }
        }

        let current_len = { self.agents.read().await.len() };
        if current_len + 1 > max_agents {
            let snapshot: Vec<(String, Arc<SubagentHandle>)> = {
                self.agents
                    .read()
                    .await
                    .iter()
                    .map(|(id, handle)| (id.clone(), Arc::clone(handle)))
                    .collect()
            };
            for (id, handle) in snapshot {
                let state = handle.state.lock().await;
                if matches!(
                    state.status,
                    SubagentStatus::Complete | SubagentStatus::Aborted | SubagentStatus::Error
                ) {
                    prune_candidates.push((state.last_update.unwrap_or(handle.created_at), id));
                }
            }
            prune_candidates.sort_by(|a, b| a.0.cmp(&b.0));

            let remove_needed = (current_len + 1).saturating_sub(max_agents);
            if remove_needed > 0 && !prune_candidates.is_empty() {
                let mut agents = self.agents.write().await;
                for (_, id) in prune_candidates.into_iter().take(remove_needed) {
                    agents.remove(&id);
                }
            }
        }

        let current_len = { self.agents.read().await.len() };
        if current_len + 1 > max_agents {
            return Err(format!(
                "too many subagents in this session (max {max_agents}); wait for some to finish or increase [subagents].max_agents"
            ));
        }

        let cancel = CancellationToken::new();
        let handle = Arc::new(SubagentHandle {
            id: agent_id.clone(),
            label: label.clone(),
            mode,
            cancel: cancel.clone(),
            notify: Notify::new(),
            state: Mutex::new(SubagentState {
                status: SubagentStatus::Queued,
                ..Default::default()
            }),
            created_at: Instant::now(),
            max_events: parent_config.subagents.max_events,
            max_event_chars: parent_config.subagents.max_event_chars,
            max_output_chars: parent_config.subagents.max_output_chars,
        });

        self.agents
            .write()
            .await
            .insert(agent_id.clone(), Arc::clone(&handle));

        tokio::spawn(run_subagent_one_shot(
            handle,
            req,
            parent_session,
            parent_turn,
            auth_manager,
            models_manager,
            skills_manager,
            parent_config,
        ));

        Ok(SubagentSpawnResponse {
            agent_id,
            status: SubagentStatus::Queued,
            label,
            mode,
        })
    }

    pub(crate) async fn poll(
        &self,
        agent_id: &str,
        await_ms: Option<u64>,
    ) -> Option<SubagentPollResponse> {
        let handle = self.agents.read().await.get(agent_id).cloned()?;
        let mut remaining = await_ms.map(Duration::from_millis);
        loop {
            let snapshot = {
                let state = handle.state.lock().await;
                SubagentPollResponse {
                    agent_id: handle.id.clone(),
                    status: state.status,
                    label: handle.label.clone(),
                    mode: handle.mode,
                    rollout_path: state.rollout_path.clone(),
                    final_output: state.final_output.clone(),
                    recent_events: state.recent_events.iter().cloned().collect(),
                }
            };

            let Some(left) = remaining else {
                return Some(snapshot);
            };
            if !matches!(
                snapshot.status,
                SubagentStatus::Queued | SubagentStatus::Running
            ) {
                return Some(snapshot);
            }

            let started = Instant::now();
            let _ = timeout(left, handle.notify.notified()).await;
            let elapsed = started.elapsed();
            remaining = left.checked_sub(elapsed);
        }
    }

    pub(crate) async fn cancel(&self, agent_id: &str) -> Option<()> {
        let handle = self.agents.read().await.get(agent_id).cloned()?;
        handle.cancel.cancel();
        Some(())
    }

    pub(crate) async fn list(&self) -> Vec<SubagentPollResponse> {
        let handles: Vec<Arc<SubagentHandle>> =
            self.agents.read().await.values().cloned().collect();
        let mut out = Vec::with_capacity(handles.len());
        for handle in handles {
            if let Some(poll) = self.poll(&handle.id, None).await {
                out.push(poll);
            }
        }
        out
    }
}

fn subagent_base_instructions(label: &str, mode: SubagentMode) -> String {
    let safety = match mode {
        SubagentMode::Explore => "- Scope: read-only exploration; do not modify files.\n",
        SubagentMode::General => {
            "- Scope: you may propose changes and (if tools are enabled) apply them.\n"
        }
    };
    format!(
        "You are a focused subagent named \"{label}\".\n\
Your job is to help the parent Codex session by producing concise, actionable results.\n\
\n\
Requirements:\n\
- Output: respond with only your final answer (no meta commentary).\n\
{safety}\
- Efficiency: keep responses short; prefer checklists and concrete next steps.\n"
    )
}

#[allow(clippy::too_many_arguments)]
async fn run_subagent_one_shot(
    handle: Arc<SubagentHandle>,
    req: SubagentSpawnRequest,
    parent_session: Arc<Session>,
    parent_turn: Arc<TurnContext>,
    auth_manager: Arc<AuthManager>,
    models_manager: Arc<ModelsManager>,
    skills_manager: Arc<SkillsManager>,
    parent_config: crate::config::Config,
) {
    let timeout_duration = req
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(parent_config.subagents.default_timeout);

    let permit = tokio::select! {
        permit = global_subagent_limiter().acquire_owned() => permit.ok(),
        _ = handle.cancel.cancelled() => None,
    };
    let Some(permit) = permit else {
        let mut state = handle.state.lock().await;
        state.status = SubagentStatus::Aborted;
        handle.notify.notify_waiters();
        return;
    };

    {
        let mut state = handle.state.lock().await;
        state.status = SubagentStatus::Running;
        state.last_update = Some(Instant::now());
        push_event(&handle, &mut state, "running".to_string());
    }
    handle.notify.notify_waiters();

    let run = timeout(timeout_duration, async {
        // Prepare per-subagent config.
        let mut config = parent_config;
        config.features.disable(Feature::Subagents);
        config.features.disable(Feature::GhostCommit);

        // Subagents are intentionally lightweight by default.
        config.project_doc_max_bytes = 0;

        config.developer_instructions = Some(match config.developer_instructions.take() {
            Some(existing) => {
                format!(
                    "{existing}\n\n{}",
                    subagent_base_instructions(&req.label, req.mode)
                )
            }
            None => subagent_base_instructions(&req.label, req.mode),
        });

        // Profile defaults.
        match req.mode {
            SubagentMode::Explore => {
                config.sandbox_policy = SandboxPolicy::new_read_only_policy();
                config.approval_policy = AskForApproval::OnRequest;
                config
                    .features
                    .disable(Feature::ApplyPatchFreeform)
                    .disable(Feature::UnifiedExec)
                    .disable(Feature::ShellTool)
                    .disable(Feature::ShellSnapshot)
                    .disable(Feature::ViewImageTool)
                    .disable(Feature::WebSearchRequest);
            }
            SubagentMode::General => {
                // Inherit parent sandbox/approval policy by default.
            }
        }

        // Seed history if resuming.
        let initial_history = if let Some(path) = &req.resume_rollout_path {
            match RolloutRecorder::get_rollout_history(path).await {
                Ok(history) => Some(history),
                Err(e) => {
                    let mut state = handle.state.lock().await;
                    state.status = SubagentStatus::Error;
                    push_event(
                        &handle,
                        &mut state,
                        format!("failed to resume subagent history: {e}"),
                    );
                    handle.notify.notify_waiters();
                    return;
                }
            }
        } else {
            None
        };

        // Resolve skills (if provided).
        if !req.skills.is_empty() {
            config.features.enable(Feature::Skills);
        }

        let CodexSpawnOk { codex, .. } = match Codex::spawn(
            config,
            auth_manager,
            models_manager,
            Arc::clone(&skills_manager),
            initial_history.unwrap_or(InitialHistory::New),
            SessionSource::SubAgent(SubAgentSource::Other(req.label.clone())),
        )
        .await
        {
            Ok(ok) => ok,
            Err(e) => {
                let mut state = handle.state.lock().await;
                state.status = SubagentStatus::Error;
                push_event(
                    &handle,
                    &mut state,
                    format!("failed to spawn subagent: {e}"),
                );
                handle.notify.notify_waiters();
                return;
            }
        };

        // Wait for SessionConfigured so we can capture rollout_path for resume/polling.
        let codex = Arc::new(codex);
        if let Ok(Some(path)) = timeout(
            SESSION_CONFIGURED_TIMEOUT,
            wait_for_session_configured(&codex),
        )
        .await
        {
            let mut state = handle.state.lock().await;
            state.rollout_path = Some(path);
            state.last_update = Some(Instant::now());
        }
        handle.notify.notify_waiters();

        let mut inputs: Vec<UserInput> = vec![UserInput::Text {
            text: req.prompt.clone(),
        }];

        if !req.skills.is_empty() {
            let outcome = skills_manager.skills_for_cwd(&parent_turn.cwd);
            for name in req.skills {
                if let Some(skill) = outcome.skills.iter().find(|s| s.name == name) {
                    inputs.push(UserInput::Skill {
                        name: skill.name.clone(),
                        path: skill.path.clone(),
                    });
                } else {
                    let mut state = handle.state.lock().await;
                    state.status = SubagentStatus::Error;
                    push_event(
                        &handle,
                        &mut state,
                        format!("unknown skill requested: {name}"),
                    );
                    handle.notify.notify_waiters();
                    return;
                }
            }
        }

        if let Err(e) = codex.submit(Op::UserInput { items: inputs }).await {
            let mut state = handle.state.lock().await;
            state.status = SubagentStatus::Error;
            push_event(
                &handle,
                &mut state,
                format!("failed to start subagent: {e}"),
            );
            handle.notify.notify_waiters();
            return;
        }

        // Drive until completion or cancellation, forwarding approvals through the parent.
        loop {
            let event: Event = tokio::select! {
                _ = handle.cancel.cancelled() => {
                    shutdown_subagent(&codex).await;
                    let mut state = handle.state.lock().await;
                    state.status = SubagentStatus::Aborted;
                    push_event(&handle, &mut state, "cancelled".to_string());
                    handle.notify.notify_waiters();
                    return;
                }
                event = codex.next_event() => match event {
                    Ok(event) => event,
                    Err(e) => {
                        let mut state = handle.state.lock().await;
                        state.status = SubagentStatus::Error;
                        push_event(&handle, &mut state, format!("subagent died: {e}"));
                        handle.notify.notify_waiters();
                        return;
                    }
                }
            };

            match event.msg {
                EventMsg::SessionConfigured(ev) => {
                    let mut state = handle.state.lock().await;
                    state.rollout_path = Some(ev.rollout_path.clone());
                    state.last_update = Some(Instant::now());
                    handle.notify.notify_waiters();
                }
                EventMsg::ExecApprovalRequest(ev) => {
                    handle_exec_approval_request(&handle, &codex, &parent_session, &event.id, ev)
                        .await;
                }
                EventMsg::ApplyPatchApprovalRequest(ev) => {
                    handle_patch_approval_request(&handle, &codex, &parent_session, &event.id, ev)
                        .await;
                }
                EventMsg::Error(ev) => {
                    let mut state = handle.state.lock().await;
                    state.status = SubagentStatus::Error;
                    state.final_output = Some(cap_output(&handle, ev.message.clone()));
                    state.last_update = Some(Instant::now());
                    push_event(&handle, &mut state, format!("error: {}", ev.message));
                    handle.notify.notify_waiters();
                }
                EventMsg::StreamError(ev) => {
                    let mut state = handle.state.lock().await;
                    state.status = SubagentStatus::Error;
                    state.final_output = Some(cap_output(&handle, ev.message.clone()));
                    state.last_update = Some(Instant::now());
                    push_event(&handle, &mut state, format!("stream error: {}", ev.message));
                    handle.notify.notify_waiters();
                }
                EventMsg::AgentMessage(ev) => {
                    let mut state = handle.state.lock().await;
                    state.last_update = Some(Instant::now());
                    push_event(&handle, &mut state, ev.message);
                    handle.notify.notify_waiters();
                }
                EventMsg::TaskComplete(tc) => {
                    let mut state = handle.state.lock().await;
                    if state.status != SubagentStatus::Error {
                        state.status = SubagentStatus::Complete;
                        state.final_output =
                            tc.last_agent_message.map(|text| cap_output(&handle, text));
                    } else if state.final_output.is_none() {
                        state.final_output =
                            tc.last_agent_message.map(|text| cap_output(&handle, text));
                    }
                    state.last_update = Some(Instant::now());
                    push_event(&handle, &mut state, "complete".to_string());
                    handle.notify.notify_waiters();
                    shutdown_subagent(&codex).await;
                    break;
                }
                EventMsg::TurnAborted(_) => {
                    let mut state = handle.state.lock().await;
                    state.status = SubagentStatus::Aborted;
                    state.last_update = Some(Instant::now());
                    push_event(&handle, &mut state, "aborted".to_string());
                    handle.notify.notify_waiters();
                    shutdown_subagent(&codex).await;
                    break;
                }
                _ => {}
            }
        }
    })
    .await;

    drop(permit);

    if run.is_err() {
        handle.cancel.cancel();
        let mut state = handle.state.lock().await;
        if state.status == SubagentStatus::Running {
            state.status = SubagentStatus::Error;
        }
        push_event(
            &handle,
            &mut state,
            format!("timed out after {}ms", timeout_duration.as_millis()),
        );
        handle.notify.notify_waiters();
    }
}

async fn wait_for_session_configured(codex: &Codex) -> Option<PathBuf> {
    loop {
        let event = codex.next_event().await.ok()?;
        // Ignore other startup chatter.
        if let EventMsg::SessionConfigured(ev) = event.msg {
            return Some(ev.rollout_path);
        }
    }
}

async fn shutdown_subagent(codex: &Codex) {
    let _ = codex.submit(Op::Interrupt).await;
    let _ = codex.submit(Op::Shutdown {}).await;
}

fn cap_output(handle: &SubagentHandle, mut message: String) -> String {
    if message.len() > handle.max_output_chars {
        truncate_to_char_boundary(&mut message, handle.max_output_chars);
    }
    message
}

fn push_event(handle: &SubagentHandle, state: &mut SubagentState, mut message: String) {
    if message.len() > handle.max_event_chars {
        truncate_to_char_boundary(&mut message, handle.max_event_chars);
    }
    if state.recent_events.len() >= handle.max_events {
        state.recent_events.pop_front();
    }
    state.recent_events.push_back(message);
}

fn truncate_to_char_boundary(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    s.truncate(idx);
}

async fn handle_exec_approval_request(
    handle: &SubagentHandle,
    codex: &Codex,
    parent_session: &Session,
    subagent_turn_id: &str,
    ev: ExecApprovalRequestEvent,
) {
    let approval_id = format!("subagent-{}-exec-{}", handle.id, subagent_turn_id);
    let decision = parent_session
        .request_command_approval_background(
            approval_id,
            ev.call_id,
            ev.command,
            ev.cwd,
            ev.reason,
            ev.proposed_execpolicy_amendment,
        )
        .await;
    let _ = codex
        .submit(Op::ExecApproval {
            id: subagent_turn_id.to_string(),
            decision: decision.clone(),
        })
        .await;
    if matches!(decision, ReviewDecision::Abort) {
        handle.cancel.cancel();
    }
}

async fn handle_patch_approval_request(
    handle: &SubagentHandle,
    codex: &Codex,
    parent_session: &Session,
    subagent_turn_id: &str,
    ev: ApplyPatchApprovalRequestEvent,
) {
    let approval_id = format!("subagent-{}-patch-{}", handle.id, subagent_turn_id);
    let decision_rx = parent_session
        .request_patch_approval_background(
            approval_id,
            ev.call_id,
            ev.changes,
            ev.reason,
            ev.grant_root,
        )
        .await;
    let decision = decision_rx.await.unwrap_or_default();
    let _ = codex
        .submit(Op::PatchApproval {
            id: subagent_turn_id.to_string(),
            decision: decision.clone(),
        })
        .await;
    if matches!(decision, ReviewDecision::Abort) {
        handle.cancel.cancel();
    }
}
