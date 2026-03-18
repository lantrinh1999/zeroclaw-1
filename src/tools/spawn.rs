use super::traits::{Tool, ToolResult};
use crate::agent::loop_::run_tool_call_loop;
use crate::config::DelegateAgentConfig;
use crate::observability::traits::{Observer, ObserverEvent, ObserverMetric};
use crate::providers::{self, ChatMessage, Provider};
use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

/// Default timeout for background sub-agent runs.
const SPAWN_AGENTIC_TIMEOUT_SECS: u64 = 600;

/// Auto-clean completed tasks older than this (10 minutes).
const GC_COMPLETED_TASK_AGE_MS: i64 = 600_000;

/// Broadcast capacity for spawn completion events.
const COMPLETION_CHANNEL_CAPACITY: usize = 64;

// ── SpawnCompletion ───────────────────────────────────────────────────────

/// Event broadcast when a spawned sub-agent completes.
#[derive(Debug, Clone)]
pub struct SpawnCompletion {
    pub task_id: String,
    pub label: String,
    pub status: SubagentStatus,
    pub result: String,
    pub reply_channel: Option<String>,
    pub reply_target: Option<String>,
    pub thread_ts: Option<String>,
}

/// Context for delivering results back to the originating channel.
#[derive(Debug, Clone)]
pub struct SpawnReplyCtx {
    pub channel_name: String,
    pub reply_target: String,
    pub thread_ts: Option<String>,
}

// ── SubagentStatus ────────────────────────────────────────────────────────

/// Status of a spawned sub-agent task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
    Canceled,
}

impl std::fmt::Display for SubagentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Canceled => write!(f, "canceled"),
        }
    }
}

// ── SubagentTask ──────────────────────────────────────────────────────────

/// A tracked background sub-agent task.
#[derive(Debug, Clone)]
pub struct SubagentTask {
    pub id: String,
    pub task: String,
    pub label: String,
    pub agent_name: String,
    pub status: SubagentStatus,
    pub result: String,
    pub created: i64,
}

// ── SubagentManager ───────────────────────────────────────────────────────

/// Manages spawned sub-agent tasks. Thread-safe via `RwLock`.
pub struct SubagentManager {
    tasks: Arc<RwLock<HashMap<String, SubagentTask>>>,
    next_id: Arc<parking_lot::Mutex<u32>>,
    pub(crate) agents: Arc<HashMap<String, DelegateAgentConfig>>,
    fallback_credential: Option<String>,
    provider_runtime_options: providers::ProviderRuntimeOptions,
    parent_tools: Arc<RwLock<Vec<Arc<dyn Tool>>>>,
    multimodal_config: crate::config::MultimodalConfig,
    completion_tx: broadcast::Sender<SpawnCompletion>,
    current_reply_ctx: Arc<RwLock<Option<SpawnReplyCtx>>>,
}

impl SubagentManager {
    pub fn new(
        agents: Arc<HashMap<String, DelegateAgentConfig>>,
        fallback_credential: Option<String>,
        provider_runtime_options: providers::ProviderRuntimeOptions,
        parent_tools: Arc<RwLock<Vec<Arc<dyn Tool>>>>,
        multimodal_config: crate::config::MultimodalConfig,
    ) -> Self {
        let (completion_tx, _) = broadcast::channel(COMPLETION_CHANNEL_CAPACITY);
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            next_id: Arc::new(parking_lot::Mutex::new(1)),
            agents: Arc::new(agents.as_ref().clone()),
            fallback_credential,
            provider_runtime_options,
            parent_tools,
            multimodal_config,
            completion_tx,
            current_reply_ctx: Arc::new(RwLock::new(None)),
        }
    }

    /// Subscribe to completion events from spawned sub-agents.
    pub fn subscribe_completions(&self) -> broadcast::Receiver<SpawnCompletion> {
        self.completion_tx.subscribe()
    }

    /// Set the reply context for the current message being processed.
    /// Called by channel/CLI handlers before running the tool loop.
    pub fn set_reply_context(&self, ctx: Option<SpawnReplyCtx>) {
        *self.current_reply_ctx.write() = ctx;
    }

    /// Capture a snapshot of the current reply context.
    fn capture_reply_context(&self) -> Option<SpawnReplyCtx> {
        self.current_reply_ctx.read().clone()
    }

    /// Spawn a background sub-agent to handle a task.
    /// Returns `(task_id, display_message)` on success.
    #[allow(clippy::cast_possible_truncation)]
    pub fn spawn(
        &self,
        task: String,
        label: String,
        agent_name: String,
    ) -> Result<(String, String), String> {
        if task.trim().is_empty() {
            return Err("task is required and must be a non-empty string".into());
        }

        // Auto-clean old completed tasks to prevent memory leak
        self.gc_completed_tasks();

        // Resolve agent config — use first agent if none specified
        let resolved_agent_name = if agent_name.trim().is_empty() {
            match self.agents.keys().next() {
                Some(name) => name.clone(),
                None => return Err("No delegate agents configured for spawn".into()),
            }
        } else {
            agent_name.trim().to_string()
        };

        let agent_config = match self.agents.get(&resolved_agent_name) {
            Some(cfg) => cfg.clone(),
            None => {
                let available: Vec<&str> = self.agents.keys().map(|s| s.as_str()).collect();
                return Err(format!(
                    "Unknown agent '{}'. Available: {}",
                    resolved_agent_name,
                    if available.is_empty() {
                        "(none)".to_string()
                    } else {
                        available.join(", ")
                    }
                ));
            }
        };

        // Generate task ID
        let task_id = {
            let mut id = self.next_id.lock();
            let current = *id;
            *id += 1;
            format!("subagent-{current}")
        };

        let now_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64; // millis won't exceed i64 until year ~292M

        let subagent_task = SubagentTask {
            id: task_id.clone(),
            task: task.clone(),
            label: label.clone(),
            agent_name: resolved_agent_name.clone(),
            status: SubagentStatus::Running,
            result: String::new(),
            created: now_millis,
        };

        self.tasks.write().insert(task_id.clone(), subagent_task);

        // Build sub-tools for the sub-agent
        let sub_tools: Vec<Box<dyn Tool>> = {
            let parent_tools = self.parent_tools.read();
            if agent_config.agentic && !agent_config.allowed_tools.is_empty() {
                let allowed: std::collections::HashSet<&str> = agent_config
                    .allowed_tools
                    .iter()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .collect();
                let is_wildcard = allowed.contains("*");
                parent_tools
                    .iter()
                    .filter(|tool| is_wildcard || allowed.contains(tool.name()))
                    .filter(|tool| tool.name() != "delegate" && tool.name() != "spawn")
                    .map(|tool| Box::new(ToolArcRef::new(tool.clone())) as Box<dyn Tool>)
                    .collect()
            } else {
                // Non-agentic or no allowlist — give basic tools
                parent_tools
                    .iter()
                    .filter(|tool| {
                        matches!(
                            tool.name(),
                            "shell"
                                | "file_read"
                                | "file_write"
                                | "file_edit"
                                | "glob_search"
                                | "content_search"
                                | "web_fetch"
                                | "web_search"
                                | "http_request"
                                | "memory_store"
                                | "memory_recall"
                        )
                    })
                    .map(|tool| Box::new(ToolArcRef::new(tool.clone())) as Box<dyn Tool>)
                    .collect()
            }
        };

        // Create provider
        let provider_credential = agent_config
            .api_key
            .clone()
            .or_else(|| self.fallback_credential.clone());
        let provider = match providers::create_provider_with_options(
            &agent_config.provider,
            provider_credential.as_deref(),
            &self.provider_runtime_options,
        ) {
            Ok(p) => p,
            Err(e) => {
                self.tasks.write().remove(&task_id);
                return Err(format!(
                    "Failed to create provider '{}': {e}",
                    agent_config.provider
                ));
            }
        };

        let tasks = Arc::clone(&self.tasks);
        let multimodal_config = self.multimodal_config.clone();
        let task_id_clone = task_id.clone();
        let task_clone = task.clone();
        let completion_tx = self.completion_tx.clone();
        let reply_ctx = self.capture_reply_context();

        // Spawn background task
        tokio::spawn(async move {
            Self::run_task(
                tasks,
                task_id_clone,
                agent_config,
                provider,
                sub_tools,
                multimodal_config,
                task_clone,
                completion_tx,
                reply_ctx,
            )
            .await;
        });

        let display = if label.is_empty() {
            format!(
                "Spawned background subagent '{task_id}' (agent: {resolved_agent_name}). \
                 Use spawn_status tool to check progress."
            )
        } else {
            format!(
                "Spawned background subagent '{task_id}' label=\"{label}\" \
                 (agent: {resolved_agent_name}). Use spawn_status tool to check progress."
            )
        };

        Ok((task_id, display))
    }

    /// Run a sub-agent task in the background.
    #[allow(clippy::too_many_arguments)]
    async fn run_task(
        tasks: Arc<RwLock<HashMap<String, SubagentTask>>>,
        task_id: String,
        agent_config: DelegateAgentConfig,
        provider: Box<dyn Provider>,
        sub_tools: Vec<Box<dyn Tool>>,
        multimodal_config: crate::config::MultimodalConfig,
        task_prompt: String,
        completion_tx: broadcast::Sender<SpawnCompletion>,
        reply_ctx: Option<SpawnReplyCtx>,
    ) {
        let mut history = Vec::new();
        let system_prompt = agent_config
            .system_prompt
            .as_deref()
            .unwrap_or(
                "You are a subagent. Complete the given task independently and report the result.\n\
                 You have access to tools - use them as needed to complete your task.\n\
                 After completing the task, provide a clear summary of what was done.",
            )
            .to_string();
        history.push(ChatMessage::system(system_prompt));
        history.push(ChatMessage::user(task_prompt));

        let noop_observer = NoopObserver;
        let temperature = agent_config.temperature.unwrap_or(0.7);
        let max_iterations = if agent_config.max_iterations == 0 {
            10
        } else {
            agent_config.max_iterations
        };

        let result = tokio::time::timeout(
            Duration::from_secs(SPAWN_AGENTIC_TIMEOUT_SECS),
            run_tool_call_loop(
                &*provider,
                &mut history,
                &sub_tools,
                &noop_observer,
                &agent_config.provider,
                &agent_config.model,
                temperature,
                true, // silent
                None, // approval
                "spawn",
                &multimodal_config,
                max_iterations,
                None, // cancellation_token
                None, // on_delta
                None, // hooks
                &[],  // excluded_tools
                &[],  // dedup_exempt_tools
                None, // activated_tools
            ),
        )
        .await;

        let (final_status, final_result, label) = {
            let mut tasks_lock = tasks.write();
            if let Some(task) = tasks_lock.get_mut(&task_id) {
                match result {
                    Ok(Ok(response)) => {
                        task.status = SubagentStatus::Completed;
                        task.result = if response.trim().is_empty() {
                            "[Empty response]".to_string()
                        } else {
                            response
                        };
                    }
                    Ok(Err(e)) => {
                        task.status = SubagentStatus::Failed;
                        task.result = format!("Error: {e}");
                    }
                    Err(_) => {
                        task.status = SubagentStatus::Failed;
                        task.result = format!("Timed out after {SPAWN_AGENTIC_TIMEOUT_SECS}s");
                    }
                }
                (task.status.clone(), task.result.clone(), task.label.clone())
            } else {
                return;
            }
        };

        // Broadcast completion event for auto-delivery
        let _ = completion_tx.send(SpawnCompletion {
            task_id,
            label,
            status: final_status,
            result: final_result,
            reply_channel: reply_ctx.as_ref().map(|c| c.channel_name.clone()),
            reply_target: reply_ctx.as_ref().map(|c| c.reply_target.clone()),
            thread_ts: reply_ctx.and_then(|c| c.thread_ts),
        });
    }

    /// Get a snapshot of a task by ID.
    pub fn get_task(&self, task_id: &str) -> Option<SubagentTask> {
        self.tasks.read().get(task_id).cloned()
    }

    /// Get snapshots of all tasks.
    pub fn list_tasks(&self) -> Vec<SubagentTask> {
        let tasks = self.tasks.read();
        let mut list: Vec<SubagentTask> = tasks.values().cloned().collect();
        list.sort_by_key(|t| t.created);
        list
    }

    /// Wait for a task to complete (polling). Returns the final task state.
    pub async fn wait_for_task(
        &self,
        task_id: &str,
        timeout: Duration,
    ) -> Option<SubagentTask> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(task) = self.get_task(task_id) {
                if task.status != SubagentStatus::Running {
                    return Some(task);
                }
            } else {
                return None;
            }
            if tokio::time::Instant::now() >= deadline {
                return self.get_task(task_id);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Remove completed/failed/canceled tasks older than `GC_COMPLETED_TASK_AGE_MS`.
    #[allow(clippy::cast_possible_truncation)]
    fn gc_completed_tasks(&self) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        self.tasks.write().retain(|_, task| {
            task.status == SubagentStatus::Running
                || (now_ms - task.created) < GC_COMPLETED_TASK_AGE_MS
        });
    }
}

// ── SpawnTool ─────────────────────────────────────────────────────────────

/// Tool that spawns a sub-agent to handle a task in the background.
pub struct SpawnTool {
    manager: Arc<SubagentManager>,
}

impl SpawnTool {
    pub fn new(manager: Arc<SubagentManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for SpawnTool {
    fn name(&self) -> &str {
        "spawn"
    }

    fn description(&self) -> &str {
        "Spawn a subagent or tasks to handle a task in the background. Use this for complex \
         or time-consuming tasks that can run independently. The subagent will \
         complete the task asynchronously. Use spawn_status tool to check progress \
         and retrieve results."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let agent_names: Vec<&str> = self.manager.agents.keys().map(|s| s.as_str()).collect();
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The task for the subagent to complete"
                },
                "label": {
                    "type": "string",
                    "description": "Optional short label for display"
                },
                "agent": {
                    "type": "string",
                    "description": format!(
                        "Agent to use. Available: {}",
                        if agent_names.is_empty() {
                            "(uses default)".to_string()
                        } else {
                            agent_names.join(", ")
                        }
                    )
                },
                "wait": {
                    "type": "boolean",
                    "description": "Wait for completion and return result inline (default: true). Set false for fire-and-forget."
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        let label = args
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        let agent = args
            .get("agent")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        let wait = args
            .get("wait")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let (task_id, display_msg) = match self.manager.spawn(task, label, agent) {
            Ok(pair) => pair,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e),
                });
            }
        };

        if !wait {
            return Ok(ToolResult {
                success: true,
                output: display_msg,
                error: None,
            });
        }

        // Wait for subagent completion (up to the spawn timeout)
        let timeout = Duration::from_secs(SPAWN_AGENTIC_TIMEOUT_SECS + 10);
        match self.manager.wait_for_task(&task_id, timeout).await {
            Some(completed_task) => {
                let success = completed_task.status == SubagentStatus::Completed;
                let output = if completed_task.result.is_empty() {
                    format!(
                        "Subagent '{}' finished with status: {}",
                        task_id, completed_task.status
                    )
                } else {
                    completed_task.result
                };
                Ok(ToolResult {
                    success,
                    output,
                    error: if success {
                        None
                    } else {
                        Some(format!("Subagent status: {}", completed_task.status))
                    },
                })
            }
            None => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Task '{task_id}' disappeared during wait")),
            }),
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

struct ToolArcRef {
    inner: Arc<dyn Tool>,
}

impl ToolArcRef {
    fn new(inner: Arc<dyn Tool>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl Tool for ToolArcRef {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.inner.execute(args).await
    }
}

struct NoopObserver;

impl Observer for NoopObserver {
    fn record_event(&self, _event: &ObserverEvent) {}

    fn record_metric(&self, _metric: &ObserverMetric) {}

    fn name(&self) -> &str {
        "spawn-noop"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_agents() -> Arc<HashMap<String, DelegateAgentConfig>> {
        Arc::new(HashMap::new())
    }

    fn sample_agents() -> Arc<HashMap<String, DelegateAgentConfig>> {
        let mut m = HashMap::new();
        m.insert(
            "researcher".to_string(),
            DelegateAgentConfig {
                provider: "ollama".to_string(),
                model: "llama3".to_string(),
                system_prompt: Some("You are a research assistant.".to_string()),
                api_key: None,
                temperature: Some(0.3),
                max_depth: 3,
                agentic: true,
                allowed_tools: vec!["shell".into(), "file_read".into()],
                max_iterations: 5,
            },
        );
        Arc::new(m)
    }

    fn make_manager(agents: Arc<HashMap<String, DelegateAgentConfig>>) -> Arc<SubagentManager> {
        Arc::new(SubagentManager::new(
            agents,
            None,
            providers::ProviderRuntimeOptions::default(),
            Arc::new(RwLock::new(Vec::new())),
            crate::config::MultimodalConfig::default(),
        ))
    }

    #[test]
    fn spawn_tool_name_and_schema() {
        let manager = make_manager(sample_agents());
        let tool = SpawnTool::new(manager);
        assert_eq!(tool.name(), "spawn");
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["task"].is_object());
        assert!(schema["properties"]["label"].is_object());
        assert!(schema["properties"]["agent"].is_object());
        assert!(schema["properties"]["wait"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert!(required.contains(&json!("task")));
    }

    #[test]
    fn spawn_tool_description_not_empty() {
        let manager = make_manager(sample_agents());
        let tool = SpawnTool::new(manager);
        assert!(!tool.description().is_empty());
        assert!(tool.description().contains("background"));
    }

    #[test]
    fn spawn_empty_task_returns_error() {
        let manager = make_manager(sample_agents());
        let result = manager.spawn(String::new(), String::new(), String::new());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("required"));
    }

    #[test]
    fn spawn_unknown_agent_returns_error() {
        let manager = make_manager(sample_agents());
        let result = manager.spawn("test task".into(), String::new(), "nonexistent".into());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown agent"));
    }

    #[test]
    fn spawn_no_agents_returns_error() {
        let manager = make_manager(empty_agents());
        let result = manager.spawn("test task".into(), String::new(), String::new());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No delegate agents"));
    }

    #[test]
    fn list_tasks_empty() {
        let manager = make_manager(sample_agents());
        assert!(manager.list_tasks().is_empty());
    }

    #[test]
    fn get_task_not_found() {
        let manager = make_manager(sample_agents());
        assert!(manager.get_task("subagent-999").is_none());
    }

    #[test]
    fn task_status_display() {
        assert_eq!(SubagentStatus::Running.to_string(), "running");
        assert_eq!(SubagentStatus::Completed.to_string(), "completed");
        assert_eq!(SubagentStatus::Failed.to_string(), "failed");
        assert_eq!(SubagentStatus::Canceled.to_string(), "canceled");
    }
}
