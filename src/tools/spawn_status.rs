use super::spawn::{SubagentManager, SubagentStatus};
use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Tool that queries the status of spawned sub-agent tasks.
pub struct SpawnStatusTool {
    manager: Arc<SubagentManager>,
}

impl SpawnStatusTool {
    pub fn new(manager: Arc<SubagentManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for SpawnStatusTool {
    fn name(&self) -> &str {
        "spawn_status"
    }

    fn description(&self) -> &str {
        "Check status and results of spawned background subagents. \
         Without arguments, lists all subagents. \
         With task_id, shows detailed status for that specific task."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Optional task ID to get specific status"
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let task_id = args
            .get("task_id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());

        if let Some(id) = task_id {
            // Return detail for a specific task
            match self.manager.get_task(id) {
                Some(task) => Ok(ToolResult {
                    success: true,
                    output: format_task_detail(&task),
                    error: None,
                }),
                None => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Task '{}' not found", id)),
                }),
            }
        } else {
            // List all tasks
            let tasks = self.manager.list_tasks();
            if tasks.is_empty() {
                return Ok(ToolResult {
                    success: true,
                    output: "No subagent tasks have been spawned.".to_string(),
                    error: None,
                });
            }

            let mut running = 0u32;
            let mut completed = 0u32;
            let mut failed = 0u32;
            let mut canceled = 0u32;

            for task in &tasks {
                match task.status {
                    SubagentStatus::Running => running += 1,
                    SubagentStatus::Completed => completed += 1,
                    SubagentStatus::Failed => failed += 1,
                    SubagentStatus::Canceled => canceled += 1,
                }
            }

            let mut output = format!(
                "Subagent Tasks Summary: {} total ({} running, {} completed, {} failed, {} canceled)\n\n",
                tasks.len(),
                running,
                completed,
                failed,
                canceled
            );

            for task in &tasks {
                let label_part = if task.label.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", task.label)
                };

                let result_preview = if task.result.is_empty() {
                    String::new()
                } else {
                    let preview = truncate_str(&task.result, 200);
                    format!("\n    Result: {preview}")
                };

                let _ = writeln!(
                    output,
                    "- {}{} (agent: {}) — {}{}",
                    task.id, label_part, task.agent_name, task.status, result_preview
                );
            }

            Ok(ToolResult {
                success: true,
                output,
                error: None,
            })
        }
    }
}

fn format_task_detail(task: &super::spawn::SubagentTask) -> String {
    let mut out = format!(
        "Task: {}\nLabel: {}\nAgent: {}\nStatus: {}\nCreated: {}\n",
        task.id,
        if task.label.is_empty() {
            "(none)"
        } else {
            &task.label
        },
        task.agent_name,
        task.status,
        format_timestamp(task.created),
    );

    if !task.result.is_empty() {
        let _ = write!(out, "Result:\n{}\n", task.result);
    }

    let _ = write!(out, "Original task: {}", task.task);

    out
}

#[allow(clippy::cast_possible_truncation)]
fn format_timestamp(millis: i64) -> String {
    let secs = millis / 1000;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let elapsed = now - secs;
    if elapsed < 60 {
        return format!("{elapsed}s ago");
    }
    if elapsed < 3600 {
        return format!("{}m ago", elapsed / 60);
    }
    format!("{}h ago", elapsed / 3600)
}

fn truncate_str(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        &s[..max_len]
    }
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_status_tool_name_and_schema() {
        let manager = Arc::new(SubagentManager::new(
            Arc::new(std::collections::HashMap::new()),
            None,
            crate::providers::ProviderRuntimeOptions::default(),
            Arc::new(parking_lot::RwLock::new(Vec::new())),
            crate::config::MultimodalConfig::default(),
        ));
        let tool = SpawnStatusTool::new(manager);
        assert_eq!(tool.name(), "spawn_status");
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["task_id"].is_object());
    }

    #[tokio::test]
    async fn spawn_status_no_tasks() {
        let manager = Arc::new(SubagentManager::new(
            Arc::new(std::collections::HashMap::new()),
            None,
            crate::providers::ProviderRuntimeOptions::default(),
            Arc::new(parking_lot::RwLock::new(Vec::new())),
            crate::config::MultimodalConfig::default(),
        ));
        let tool = SpawnStatusTool::new(manager);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No subagent tasks"));
    }

    #[tokio::test]
    async fn spawn_status_task_not_found() {
        let manager = Arc::new(SubagentManager::new(
            Arc::new(std::collections::HashMap::new()),
            None,
            crate::providers::ProviderRuntimeOptions::default(),
            Arc::new(parking_lot::RwLock::new(Vec::new())),
            crate::config::MultimodalConfig::default(),
        ));
        let tool = SpawnStatusTool::new(manager);
        let result = tool
            .execute(json!({"task_id": "subagent-999"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("not found"));
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn format_timestamp_seconds() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64; // safe: won't overflow for ~292M years
        let formatted = format_timestamp(now - 30_000); // 30s ago
        assert!(formatted.contains("30s ago"));
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn format_timestamp_minutes() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64; // safe: won't overflow for ~292M years
        let formatted = format_timestamp(now - 120_000); // 2m ago
        assert!(formatted.contains("2m ago"));
    }

    #[test]
    fn truncate_str_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_long() {
        assert_eq!(truncate_str("hello world", 5), "hello");
    }
}
