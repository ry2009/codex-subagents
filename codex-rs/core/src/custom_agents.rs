use crate::git_info::resolve_root_git_project_for_trust;
use crate::subagents::SubagentMode;
use dunce::canonicalize as normalize_path;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use tokio::fs;

const AGENTS_DIR_NAME: &str = "agents";
const REPO_ROOT_CONFIG_DIR_NAME: &str = ".codex";
const MAX_NAME_LEN: usize = 64;
const MAX_DESCRIPTION_LEN: usize = 1024;
const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_ALLOWED_TOOLS: usize = 128;
const MAX_TOOL_NAME_LEN: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentScope {
    User,
    Repo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentToolsPolicy {
    /// No extra restrictions (inherit the parent session's configured tools).
    Inherit,
    /// Disable all tools.
    None,
    /// Restrict the tool registry to this allowlist of tool names.
    Allowlist(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CustomAgent {
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) path: PathBuf,
    pub(crate) scope: AgentScope,
    pub(crate) model: Option<String>,
    pub(crate) mode: Option<SubagentMode>,
    pub(crate) tools: AgentToolsPolicy,
    pub(crate) prompt: String,
}

#[derive(Debug, Default)]
pub(crate) struct AgentLoadOutcome {
    pub(crate) agents: Vec<CustomAgent>,
    pub(crate) errors: Vec<AgentLoadError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentLoadError {
    pub(crate) path: PathBuf,
    pub(crate) message: String,
}

#[derive(Debug, Deserialize)]
struct AgentFrontmatter {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    tools: Option<serde_yaml::Value>,
}

fn sanitize_agent_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut out = String::new();
    for ch in trimmed.chars() {
        if out.len() >= MAX_NAME_LEN {
            break;
        }
        match ch {
            'a'..='z' | '0'..='9' | '-' | '_' => out.push(ch),
            'A'..='Z' => out.push(ch.to_ascii_lowercase()),
            ' ' | '/' | ':' => out.push('-'),
            _ => {}
        }
    }

    if out.is_empty() { None } else { Some(out) }
}

fn sanitize_description(raw: Option<String>) -> Option<String> {
    let text = raw?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out = trimmed.to_string();
    if out.len() > MAX_DESCRIPTION_LEN {
        out.truncate(MAX_DESCRIPTION_LEN);
    }
    Some(out)
}

fn sanitize_model(raw: Option<String>) -> Option<String> {
    let text = raw?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

fn parse_mode(raw: Option<String>) -> Option<SubagentMode> {
    let text = raw?;
    SubagentMode::from_str(&text)
}

fn parse_tools_policy(raw: Option<serde_yaml::Value>) -> AgentToolsPolicy {
    let Some(value) = raw else {
        return AgentToolsPolicy::Inherit;
    };

    match value {
        serde_yaml::Value::Bool(false) => AgentToolsPolicy::None,
        serde_yaml::Value::Bool(true) => AgentToolsPolicy::Inherit,
        serde_yaml::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "" => AgentToolsPolicy::Inherit,
            "inherit" | "default" | "all" => AgentToolsPolicy::Inherit,
            "none" | "off" | "disabled" | "read-only" | "readonly" => AgentToolsPolicy::None,
            _ => AgentToolsPolicy::Inherit,
        },
        serde_yaml::Value::Sequence(items) => {
            let mut out: Vec<String> = Vec::new();
            for item in items.into_iter().take(MAX_ALLOWED_TOOLS) {
                let serde_yaml::Value::String(tool) = item else {
                    continue;
                };
                let trimmed = tool.trim();
                if trimmed.is_empty() || trimmed.len() > MAX_TOOL_NAME_LEN {
                    continue;
                }
                out.push(trimmed.to_ascii_lowercase());
            }
            if out.is_empty() {
                AgentToolsPolicy::Inherit
            } else {
                AgentToolsPolicy::Allowlist(out)
            }
        }
        _ => AgentToolsPolicy::Inherit,
    }
}

fn sanitize_prompt(mut prompt: String) -> String {
    if prompt.len() > MAX_PROMPT_BYTES {
        prompt.truncate(MAX_PROMPT_BYTES);
        while !prompt.is_char_boundary(prompt.len()) {
            prompt.pop();
        }
    }
    prompt
}

fn split_frontmatter(content: &str) -> Option<(String, String)> {
    let mut segments = content.split_inclusive('\n');
    let Some(first_segment) = segments.next() else {
        return None;
    };
    let first_line = first_segment.trim_end_matches(['\r', '\n']);
    if first_line.trim() != "---" {
        return None;
    }

    let mut frontmatter = String::new();
    let mut consumed = first_segment.len();
    let mut closed = false;

    for segment in segments {
        let line = segment.trim_end_matches(['\r', '\n']);
        let trimmed = line.trim();
        if trimmed == "---" {
            closed = true;
            consumed += segment.len();
            break;
        }
        frontmatter.push_str(line);
        frontmatter.push('\n');
        consumed += segment.len();
    }

    if !closed {
        return None;
    }

    let body = if consumed >= content.len() {
        String::new()
    } else {
        content[consumed..].to_string()
    };

    Some((frontmatter, body))
}

async fn load_agent_from_path(path: &Path, scope: AgentScope) -> Result<CustomAgent, String> {
    let is_md = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("md"))
        .unwrap_or(false);
    if !is_md {
        return Err("not a markdown file".to_string());
    }

    let file_stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "invalid filename".to_string())?;

    let content = fs::read_to_string(path)
        .await
        .map_err(|e| format!("failed to read: {e}"))?;

    let (frontmatter_text, body) = match split_frontmatter(&content) {
        Some((fm, body)) => (Some(fm), body),
        None => (None, content),
    };

    let frontmatter: AgentFrontmatter = if let Some(frontmatter_text) = frontmatter_text {
        serde_yaml::from_str(&frontmatter_text)
            .map_err(|e| format!("invalid YAML frontmatter: {e}"))?
    } else {
        AgentFrontmatter {
            name: None,
            description: None,
            role: None,
            model: None,
            mode: None,
            tools: None,
        }
    };

    let name = frontmatter
        .name
        .as_deref()
        .and_then(sanitize_agent_name)
        .or_else(|| sanitize_agent_name(file_stem))
        .ok_or_else(|| "missing or invalid agent name".to_string())?;

    let description = sanitize_description(frontmatter.description.or(frontmatter.role));
    let model = sanitize_model(frontmatter.model);
    let mode = parse_mode(frontmatter.mode);
    let tools = parse_tools_policy(frontmatter.tools);

    Ok(CustomAgent {
        name,
        description,
        path: path.to_path_buf(),
        scope,
        model,
        mode,
        tools,
        prompt: sanitize_prompt(body),
    })
}

fn user_agents_root(codex_home: &Path) -> PathBuf {
    codex_home.join(AGENTS_DIR_NAME)
}

fn repo_agents_root(cwd: &Path) -> Option<PathBuf> {
    resolve_root_git_project_for_trust(cwd).map(|repo_root| {
        repo_root
            .join(REPO_ROOT_CONFIG_DIR_NAME)
            .join(AGENTS_DIR_NAME)
    })
}

pub(crate) async fn discover_agents(config: &crate::config::Config) -> AgentLoadOutcome {
    let mut out = AgentLoadOutcome::default();
    let mut by_name: BTreeMap<String, CustomAgent> = BTreeMap::new();

    let mut roots: Vec<(AgentScope, PathBuf)> =
        vec![(AgentScope::User, user_agents_root(&config.codex_home))];
    if let Some(repo_root) = repo_agents_root(&config.cwd) {
        roots.push((AgentScope::Repo, repo_root));
    }

    for (scope, root) in roots {
        let Ok(root) = normalize_path(root) else {
            continue;
        };
        let mut entries = match fs::read_dir(&root).await {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let is_file_like = fs::metadata(&path)
                .await
                .map(|m| m.is_file())
                .unwrap_or(false);
            if !is_file_like {
                continue;
            }

            match load_agent_from_path(&path, scope).await {
                Ok(agent) => {
                    match by_name.entry(agent.name.clone()) {
                        std::collections::btree_map::Entry::Vacant(v) => {
                            v.insert(agent);
                        }
                        std::collections::btree_map::Entry::Occupied(mut e) => {
                            // Repo agents override user agents with the same name.
                            if scope == AgentScope::Repo {
                                e.insert(agent);
                            }
                        }
                    };
                }
                Err(err) => {
                    out.errors.push(AgentLoadError { path, message: err });
                }
            }
        }
    }

    out.agents = by_name.into_values().collect();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    #[tokio::test]
    async fn discovers_agents_from_repo_dir() {
        let tmp = TempDir::new().expect("TempDir");
        let out = Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .expect("git init");
        assert!(out.status.success());
        fs::create_dir_all(tmp.path().join(".codex/agents")).unwrap();
        fs::write(
            tmp.path().join(".codex/agents/repo-scout.md"),
            "---\ndescription: repo agent\nmode: explore\ntools: none\n---\nHello",
        )
        .unwrap();

        let mut cfg = test_config();
        cfg.cwd = tmp.path().to_path_buf();
        cfg.codex_home = tmp.path().join("home");

        let found = discover_agents(&cfg).await;
        assert_eq!(found.errors, Vec::<AgentLoadError>::new());
        assert_eq!(found.agents.len(), 1);
        assert_eq!(found.agents[0].name, "repo-scout");
        assert_eq!(found.agents[0].description.as_deref(), Some("repo agent"));
        assert_eq!(found.agents[0].mode, Some(SubagentMode::Explore));
        assert_eq!(found.agents[0].tools, AgentToolsPolicy::None);
    }

    #[tokio::test]
    async fn repo_overrides_user_agent_with_same_name() {
        let tmp = TempDir::new().expect("TempDir");
        let out = Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .expect("git init");
        assert!(out.status.success());
        fs::create_dir_all(tmp.path().join(".codex/agents")).unwrap();
        fs::create_dir_all(tmp.path().join("home/agents")).unwrap();

        fs::write(tmp.path().join("home/agents/a.md"), "user").unwrap();
        fs::write(tmp.path().join(".codex/agents/a.md"), "repo").unwrap();

        let mut cfg = test_config();
        cfg.cwd = tmp.path().to_path_buf();
        cfg.codex_home = tmp.path().join("home");

        let found = discover_agents(&cfg).await;
        assert_eq!(found.agents.len(), 1);
        assert_eq!(
            found.agents[0].path,
            normalize_path(tmp.path().join(".codex/agents/a.md")).expect("canonicalize")
        );
    }

    #[tokio::test]
    async fn tools_allowlist_parses() {
        let tmp = TempDir::new().expect("TempDir");
        let file = tmp.path().join("a.md");
        fs::write(
            &file,
            "---\nname: a\ntools:\n  - read_file\n  - list_dir\n---\nbody",
        )
        .unwrap();

        let agent = load_agent_from_path(&file, AgentScope::Repo).await.unwrap();
        assert_eq!(
            agent.tools,
            AgentToolsPolicy::Allowlist(vec!["read_file".to_string(), "list_dir".to_string()])
        );
    }
}
