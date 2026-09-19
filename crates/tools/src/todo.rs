use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::Deserialize;
use serde_json::json;

use crate::ToolSpec;

const MAX_TODOS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// Stable per-task identifier, derived from plan/ledger order (1-based).
#[must_use]
pub fn task_id(index: usize) -> String {
    format!("t{}", index + 1)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
}

/// Shared plan ledger behind the `todo_write` tool. The runtime reads the
/// unfinished count to decide whether the agent should keep iterating on its
/// own plan instead of ending the turn.
#[derive(Default)]
pub struct TodoLedger {
    items: Mutex<Vec<TodoItem>>,
}

impl TodoLedger {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the whole list; the model always writes the full plan.
    /// Returns a compact transcript summary.
    pub fn write(&self, input: &str) -> Result<String, String> {
        let parsed: TodoWriteInput =
            serde_json::from_str(input).map_err(|error| format!("invalid todo input: {error}"))?;
        let items = parsed.into_items()?;
        if items.len() > MAX_TODOS {
            return Err(format!("todo list exceeds {MAX_TODOS} items"));
        }
        let summary = render_summary(&items);
        *self.lock() = items;
        Ok(summary)
    }

    /// Tasks not yet completed; drives loop continuation.
    #[must_use]
    pub fn pending_tasks(&self) -> usize {
        self.lock()
            .iter()
            .filter(|item| item.status != TodoStatus::Completed)
            .count()
    }

    /// Status of one task by id; used by the task-loop orchestrator to verify
    /// a task finished without inspecting the whole ledger.
    #[must_use]
    pub fn status_of(&self, id: &str) -> Option<TodoStatus> {
        self.lock()
            .iter()
            .find(|item| item.id == id)
            .map(|item| item.status)
    }

    /// Clone of the current ledger, for the orchestrator to build reflections.
    #[must_use]
    pub fn snapshot(&self) -> Vec<TodoItem> {
        self.lock().clone()
    }

    fn lock(&self) -> MutexGuard<'_, Vec<TodoItem>> {
        self.items.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Deserialize)]
struct TodoWriteInput {
    todos: Vec<TodoEntry>,
}

#[derive(Deserialize)]
struct TodoEntry {
    #[serde(default)]
    id: Option<String>,
    content: String,
    status: String,
}

impl TodoWriteInput {
    fn into_items(self) -> Result<Vec<TodoItem>, String> {
        self.todos
            .into_iter()
            .enumerate()
            .map(|(index, entry)| {
                let content = entry.content.trim().to_string();
                if content.is_empty() {
                    return Err(String::from("todo content must not be empty"));
                }
                let status = match entry.status.as_str() {
                    "pending" => TodoStatus::Pending,
                    "in_progress" => TodoStatus::InProgress,
                    "completed" => TodoStatus::Completed,
                    other => return Err(format!("unknown todo status: {other}")),
                };
                // Prefer a supplied id; otherwise derive a stable positional one
                // so the task-loop orchestrator can track the same task across
                // the plan document, the ledger, and its reflection record.
                let id = entry
                    .id
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| task_id(index));
                Ok(TodoItem {
                    id,
                    content,
                    status,
                })
            })
            .collect()
    }
}

fn render_summary(items: &[TodoItem]) -> String {
    let count = |status: TodoStatus| items.iter().filter(|item| item.status == status).count();
    let mut lines = vec![format!(
        "Todo list updated: {} items ({} in progress, {} pending, {} completed)",
        items.len(),
        count(TodoStatus::InProgress),
        count(TodoStatus::Pending),
        count(TodoStatus::Completed),
    )];
    for item in items {
        let mark = match item.status {
            TodoStatus::Pending => "[ ]",
            TodoStatus::InProgress => "[~]",
            TodoStatus::Completed => "[x]",
        };
        lines.push(format!("{mark} [{}] {}", item.id, item.content));
    }
    lines.join("\n")
}

#[must_use]
pub fn todo_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "todo_write",
        description: "Maintain the plan for multi-step work. Write the full task list (task setting), keep exactly one task in_progress, mark tasks completed as you finish them, and keep going until every task is completed.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "content": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"]
                            }
                        },
                        "required": ["content", "status"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["todos"],
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{task_id, todo_tool_spec, TodoLedger, TodoStatus};

    #[test]
    fn writes_ledger_and_counts_unfinished_tasks() {
        let ledger = TodoLedger::new();
        let summary = ledger
            .write(
                r#"{"todos":[
                    {"content":"set up env","status":"completed"},
                    {"content":"write code","status":"in_progress"},
                    {"content":"add tests","status":"pending"}
                ]}"#,
            )
            .expect("valid input should be accepted");

        assert!(summary.contains("3 items (1 in progress, 1 pending, 1 completed)"));
        assert!(summary.contains("[~] [t2] write code"));
        assert_eq!(ledger.pending_tasks(), 2);
    }

    #[test]
    fn ids_default_by_position_and_honor_supplied() {
        let ledger = TodoLedger::new();
        ledger
            .write(
                r#"{"todos":[
                    {"id":"custom","content":"a","status":"pending"},
                    {"content":"b","status":"pending"}
                ]}"#,
            )
            .expect("valid input");
        let snap = ledger.snapshot();
        assert_eq!(snap[0].id, "custom");
        assert_eq!(snap[1].id, task_id(1));
        assert_eq!(snap[1].id, "t2");
    }

    #[test]
    fn status_of_tracks_completion() {
        let ledger = TodoLedger::new();
        ledger
            .write(r#"{"todos":[{"id":"t1","content":"a","status":"in_progress"}]}"#)
            .expect("seed");
        assert_eq!(ledger.status_of("t1"), Some(TodoStatus::InProgress));
        ledger
            .write(r#"{"todos":[{"id":"t1","content":"a","status":"completed"}]}"#)
            .expect("complete");
        assert_eq!(ledger.status_of("t1"), Some(TodoStatus::Completed));
        assert_eq!(ledger.status_of("missing"), None);
        assert_eq!(ledger.pending_tasks(), 0);
    }

    #[test]
    fn rejects_invalid_entries() {
        let ledger = TodoLedger::new();
        let error = ledger
            .write(r#"{"todos":[{"content":"x","status":"done"}]}"#)
            .expect_err("unknown status should be rejected");
        assert!(error.contains("unknown todo status"));

        let error = ledger
            .write(r#"{"todos":[{"content":"   ","status":"pending"}]}"#)
            .expect_err("blank content should be rejected");
        assert!(error.contains("must not be empty"));
    }

    #[test]
    fn empty_list_clears_the_plan() {
        let ledger = TodoLedger::new();
        ledger
            .write(r#"{"todos":[{"content":"a","status":"pending"}]}"#)
            .expect("seed ledger");
        ledger.write(r#"{"todos":[]}"#).expect("clear ledger");
        assert_eq!(ledger.pending_tasks(), 0);
    }

    #[test]
    fn completed_items_do_not_block_the_loop() {
        let ledger = TodoLedger::new();
        ledger
            .write(r#"{"todos":[{"content":"a","status":"completed"}]}"#)
            .expect("write completed");
        assert_eq!(ledger.pending_tasks(), 0);
    }

    #[test]
    fn spec_advertises_the_schema() {
        let spec = todo_tool_spec();
        assert_eq!(spec.name, "todo_write");
        assert_eq!(spec.input_schema["required"][0], "todos");
        assert_eq!(
            spec.input_schema["properties"]["todos"]["items"]["properties"]["status"]["enum"][0],
            "pending"
        );
    }
}
