use crate::config::AppConfig;
use crate::model::{parse_tags, Priority, Status};
use crate::ops::{self, AddTaskInput, TaskFilter, UpdateTaskInput};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

const DEFAULT_PROTOCOL_VERSION: &str = "2025-03-26";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub struct McpServer {
    storage_path: PathBuf,
    config: AppConfig,
}

impl McpServer {
    pub fn new(storage_path: PathBuf, config: AppConfig) -> Self {
        Self {
            storage_path,
            config,
        }
    }

    pub fn serve_stdio(&self) -> io::Result<()> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        self.serve(stdin.lock(), stdout.lock())
    }

    pub fn serve<R: BufRead, W: Write>(&self, reader: R, mut writer: W) -> io::Result<()> {
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(reply) = self.handle_line(&line) {
                writeln!(writer, "{reply}")?;
                writer.flush()?;
            }
        }
        Ok(())
    }

    pub fn handle_line(&self, line: &str) -> Option<String> {
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(err) => {
                return Some(
                    json_rpc_error(Value::Null, -32700, format!("Parse error: {err}")).to_string(),
                );
            }
        };

        let id = req.get("id").cloned();
        let method = match req.get("method").and_then(Value::as_str) {
            Some(m) => m,
            None => {
                // Responses or malformed messages without a method
                return id.map(|id| {
                    json_rpc_error(id, -32600, "Invalid Request: missing method".into()).to_string()
                });
            }
        };

        // JSON-RPC notifications have no "id" and must not receive a response.
        let id = id?;

        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

        let resp = match method {
            "initialize" => self.handle_initialize(id, &params),
            "ping" => json_rpc_result(id, json!({})),
            "tools/list" => json_rpc_result(id, json!({ "tools": tool_definitions() })),
            "tools/call" => self.handle_tool_call(id, &params),
            "resources/list" => json_rpc_result(id, json!({ "resources": resource_definitions() })),
            "resources/read" => self.handle_resource_read(id, &params),
            "prompts/list" => json_rpc_result(id, json!({ "prompts": [] })),
            other => json_rpc_error(id, -32601, format!("Method not found: {other}")),
        };

        Some(resp.to_string())
    }

    fn handle_initialize(&self, id: Value, params: &Value) -> Value {
        let requested = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_PROTOCOL_VERSION);
        let negotiated = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
            requested
        } else {
            DEFAULT_PROTOCOL_VERSION
        };

        json_rpc_result(
            id,
            json!({
                "protocolVersion": negotiated,
                "capabilities": {
                    "tools": {},
                    "resources": {}
                },
                "serverInfo": {
                    "name": "roke",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "instructions": "roke is a terminal todo list and 3-column Kanban board (Todo, InProgress, Done). Use list_tasks, add_task, update_task, delete_task, list_tags, and sync_tasks to manage tasks. Prefer passing a task's stable `uid` (or numeric `id`) when updating or deleting. A task's `description` field holds optional Markdown notes (headings, checklists, code blocks, links) that users can also open in $EDITOR. Changes saved here appear live in any open roke TUI."
            }),
        )
    }

    fn handle_tool_call(&self, id: Value, params: &Value) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return json_rpc_error(id, -32602, "Invalid params: missing tool 'name'".into());
        };
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        let outcome = match name {
            "list_tasks" => self.tool_list_tasks(&args),
            "get_task" => self.tool_get_task(&args),
            "add_task" => self.tool_add_task(&args),
            "update_task" => self.tool_update_task(&args),
            "delete_task" => self.tool_delete_task(&args),
            "list_tags" => self.tool_list_tags(),
            "sync_tasks" => self.tool_sync_tasks(),
            other => {
                return json_rpc_error(id, -32602, format!("Unknown tool: '{other}'"));
            }
        };

        match outcome {
            Ok(payload) => {
                let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into());
                json_rpc_result(
                    id,
                    json!({
                        "content": [{ "type": "text", "text": text }]
                    }),
                )
            }
            Err(err) => json_rpc_result(
                id,
                json!({
                    "content": [{ "type": "text", "text": err }],
                    "isError": true
                }),
            ),
        }
    }

    fn handle_resource_read(&self, id: Value, params: &Value) -> Value {
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            return json_rpc_error(id, -32602, "Invalid params: missing resource 'uri'".into());
        };

        let content = match uri {
            "roke://tasks" => ops::board_snapshot(
                &self.storage_path,
                &self.config,
                &TaskFilter::default(),
            )
            .and_then(|snap| serde_json::to_string_pretty(&snap).map_err(|e| e.to_string())),
            "roke://tags" => ops::list_tags(&self.storage_path, &self.config)
                .and_then(|tags| serde_json::to_string_pretty(&tags).map_err(|e| e.to_string())),
            other => {
                return json_rpc_error(id, -32602, format!("Unknown resource URI: '{other}'"));
            }
        };

        match content {
            Ok(text) => json_rpc_result(
                id,
                json!({
                    "contents": [{
                        "uri": uri,
                        "mimeType": "application/json",
                        "text": text
                    }]
                }),
            ),
            Err(err) => json_rpc_error(id, -32603, err),
        }
    }

    fn tool_list_tasks(&self, args: &Value) -> Result<Value, String> {
        let status = match optional_str(args, "status")? {
            Some(s) => Some(Status::parse(s)?),
            None => None,
        };
        let priority = match optional_str(args, "priority")? {
            Some(p) => Some(Priority::parse(p)?),
            None => None,
        };
        let tags = optional_tags(args, "tags")?;
        let tag_match_all = match optional_str(args, "tag_match")? {
            None | Some("all") | Some("and") | Some("ALL") | Some("AND") => true,
            Some("any") | Some("or") | Some("ANY") | Some("OR") => false,
            Some(other) => {
                return Err(format!(
                    "invalid tag_match '{other}' (expected 'all' or 'any')"
                ));
            }
        };
        let search = optional_str(args, "search")?.map(str::to_string);

        let filter = TaskFilter {
            status,
            priority,
            tags,
            tag_match_all,
            search,
        };
        let snapshot = ops::board_snapshot(&self.storage_path, &self.config, &filter)?;
        serde_json::to_value(snapshot).map_err(|e| e.to_string())
    }

    fn tool_get_task(&self, args: &Value) -> Result<Value, String> {
        let selector = extract_selector(args)?;
        let task = ops::get_task(&self.storage_path, &self.config, &selector)?;
        serde_json::to_value(task).map_err(|e| e.to_string())
    }

    fn tool_add_task(&self, args: &Value) -> Result<Value, String> {
        let title = args
            .get("title")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing required argument 'title'".to_string())?
            .to_string();
        let description = optional_str(args, "description")?.unwrap_or("").to_string();
        let status = match optional_str(args, "status")? {
            Some(s) => Status::parse(s)?,
            None => Status::Todo,
        };
        let priority = match optional_str(args, "priority")? {
            Some(p) => Priority::parse(p)?,
            None => Priority::Medium,
        };
        let tags = optional_tags(args, "tags")?;
        let origin = optional_str(args, "origin")?.map(str::to_string);

        let task = ops::add_task(
            &self.storage_path,
            &self.config,
            AddTaskInput {
                title,
                description,
                status,
                priority,
                tags,
                origin,
            },
        )?;
        serde_json::to_value(task).map_err(|e| e.to_string())
    }

    fn tool_update_task(&self, args: &Value) -> Result<Value, String> {
        let selector = extract_selector(args)?;
        let title = optional_str(args, "title")?.map(str::to_string);
        let description = optional_str(args, "description")?.map(str::to_string);
        let status = match optional_str(args, "status")? {
            Some(s) => Some(Status::parse(s)?),
            None => None,
        };
        let priority = match optional_str(args, "priority")? {
            Some(p) => Some(Priority::parse(p)?),
            None => None,
        };
        let tags = if args.get("tags").is_some() && !args.get("tags").unwrap().is_null() {
            Some(optional_tags(args, "tags")?)
        } else {
            None
        };
        let add_tags = optional_tags(args, "add_tags")?;
        let remove_tags = optional_tags(args, "remove_tags")?;

        let updated = ops::update_task(
            &self.storage_path,
            &self.config,
            &selector,
            UpdateTaskInput {
                title,
                description,
                status,
                priority,
                tags,
                add_tags,
                remove_tags,
            },
        )?;
        serde_json::to_value(updated).map_err(|e| e.to_string())
    }

    fn tool_delete_task(&self, args: &Value) -> Result<Value, String> {
        let selector = extract_selector(args)?;
        let removed = ops::delete_task(&self.storage_path, &self.config, &selector)?;
        Ok(json!({ "deleted": removed }))
    }

    fn tool_list_tags(&self) -> Result<Value, String> {
        let tags = ops::list_tags(&self.storage_path, &self.config)?;
        Ok(json!({ "tags": tags }))
    }

    fn tool_sync_tasks(&self) -> Result<Value, String> {
        let report = ops::sync_tasks(&self.storage_path, &self.config)?;
        serde_json::to_value(report).map_err(|e| e.to_string())
    }
}

fn json_rpc_result(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn json_rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

fn optional_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(format!("argument '{key}' must be a string")),
    }
}

fn optional_tags(args: &Value, key: &str) -> Result<Vec<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(parse_tags(s)),
        Some(Value::Array(items)) => {
            let mut tags = Vec::new();
            for item in items {
                let s = item
                    .as_str()
                    .ok_or_else(|| format!("argument '{key}' items must be strings"))?;
                tags.extend(parse_tags(s));
            }
            Ok(tags)
        }
        Some(_) => Err(format!(
            "argument '{key}' must be an array of strings or a comma-separated string"
        )),
    }
}

fn extract_selector(args: &Value) -> Result<String, String> {
    if let Some(uid) = args.get("uid").and_then(Value::as_str) {
        if !uid.trim().is_empty() {
            return Ok(uid.trim().to_string());
        }
    }
    if let Some(id_val) = args.get("id") {
        if let Some(n) = id_val.as_u64() {
            return Ok(n.to_string());
        }
        if let Some(s) = id_val.as_str() {
            if !s.trim().is_empty() {
                return Ok(s.trim().to_string());
            }
        }
    }
    Err("missing required task identifier: provide 'uid' or 'id'".into())
}

fn resource_definitions() -> Value {
    json!([
        {
            "uri": "roke://tasks",
            "name": "Tasks",
            "description": "Current roke task list and Kanban status counts as JSON",
            "mimeType": "application/json"
        },
        {
            "uri": "roke://tags",
            "name": "Tags",
            "description": "All tags currently in use across tasks with counts",
            "mimeType": "application/json"
        }
    ])
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "list_tasks",
            "description": "List tasks from the roke board with optional filtering by status, priority, tags, and search text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["todo", "in-progress", "done"],
                        "description": "Filter by task status."
                    },
                    "priority": {
                        "type": "string",
                        "enum": ["low", "medium", "high"],
                        "description": "Filter by task priority."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Filter by one or more tags."
                    },
                    "tag_match": {
                        "type": "string",
                        "enum": ["all", "any"],
                        "description": "How to match multiple tags: 'all' (AND, default) or 'any' (OR)."
                    },
                    "search": {
                        "type": "string",
                        "description": "Case-insensitive substring search across title, description, and tags."
                    }
                }
            }
        },
        {
            "name": "get_task",
            "description": "Fetch a single task by its stable uid or numeric id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uid": {
                        "type": "string",
                        "description": "Stable task UID (preferred)."
                    },
                    "id": {
                        "oneOf": [{ "type": "integer" }, { "type": "string" }],
                        "description": "Numeric task ID (or UID string)."
                    }
                }
            }
        },
        {
            "name": "add_task",
            "description": "Add a new task to the roke board.",
            "inputSchema": {
                "type": "object",
                "required": ["title"],
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Short task title (required)."
                    },
                    "description": {
                        "type": "string",
                        "description": "Optional Markdown notes (supports headings, checklists, links, code blocks)."
                    },
                    "status": {
                        "type": "string",
                        "enum": ["todo", "in-progress", "done"],
                        "description": "Initial status column (default: todo)."
                    },
                    "priority": {
                        "type": "string",
                        "enum": ["low", "medium", "high"],
                        "description": "Task priority (default: medium)."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of tags (normalized to lowercase)."
                    },
                    "origin": {
                        "type": "string",
                        "description": "Optional origin label (defaults to local device name)."
                    }
                }
            }
        },
        {
            "name": "update_task",
            "description": "Update an existing task's title, description, status, priority, or tags.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uid": {
                        "type": "string",
                        "description": "Stable task UID (preferred)."
                    },
                    "id": {
                        "oneOf": [{ "type": "integer" }, { "type": "string" }],
                        "description": "Numeric task ID (or UID string)."
                    },
                    "title": {
                        "type": "string",
                        "description": "New task title."
                    },
                    "description": {
                        "type": "string",
                        "description": "New Markdown notes for the task."
                    },
                    "status": {
                        "type": "string",
                        "enum": ["todo", "in-progress", "done"],
                        "description": "Move task to a new status."
                    },
                    "priority": {
                        "type": "string",
                        "enum": ["low", "medium", "high"],
                        "description": "New task priority."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Replace all tags with this list."
                    },
                    "add_tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Add tags without removing existing ones."
                    },
                    "remove_tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Remove specific tags."
                    }
                }
            }
        },
        {
            "name": "delete_task",
            "description": "Delete a task by uid or id and record a sync tombstone.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uid": {
                        "type": "string",
                        "description": "Stable task UID (preferred)."
                    },
                    "id": {
                        "oneOf": [{ "type": "integer" }, { "type": "string" }],
                        "description": "Numeric task ID (or UID string)."
                    }
                }
            }
        },
        {
            "name": "list_tags",
            "description": "List all tags currently used on the board along with task counts.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "sync_tasks",
            "description": "Trigger two-way SSH sync with peers configured in roke's config.toml.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

    fn temp_server(name: &str) -> (McpServer, PathBuf) {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("roke_mcp_{name}_{}_{seq}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("todos.jsonl");
        (McpServer::new(path, AppConfig::default()), dir)
    }

    fn call(server: &McpServer, req: Value) -> Value {
        let reply = server.handle_line(&req.to_string()).expect("expected reply");
        serde_json::from_str(&reply).unwrap()
    }

    #[test]
    fn mcp_lifecycle_tools_and_resources() {
        let (server, dir) = temp_server("full");

        // 1. Initialize
        let init = call(
            &server,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": { "name": "test-client", "version": "1.0" }
                }
            }),
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "roke");

        // 2. Notification produces no response
        assert!(server
            .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none());

        // 3. tools/list
        let tools = call(
            &server,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        );
        let tool_names: Vec<&str> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(tool_names.contains(&"add_task"));
        assert!(tool_names.contains(&"list_tasks"));
        assert!(tool_names.contains(&"update_task"));
        assert!(tool_names.contains(&"delete_task"));

        // 4. add_task
        let added = call(
            &server,
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "add_task",
                    "arguments": {
                        "title": "Build MCP support",
                        "priority": "high",
                        "tags": ["agent", "mcp"]
                    }
                }
            }),
        );
        assert!(added["result"]["isError"].is_null());
        let added_task: Value =
            serde_json::from_str(added["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let uid = added_task["uid"].as_str().unwrap().to_string();
        assert_eq!(added_task["title"], "Build MCP support");
        assert_eq!(added_task["priority"], "High");

        // 5. update_task by uid
        let updated = call(
            &server,
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "update_task",
                    "arguments": {
                        "uid": uid,
                        "status": "in-progress",
                        "add_tags": ["wip"]
                    }
                }
            }),
        );
        let updated_task: Value =
            serde_json::from_str(updated["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(updated_task["status"], "InProgress");
        assert_eq!(updated_task["tags"], json!(["agent", "mcp", "wip"]));

        // 6. resources/read roke://tasks
        let res = call(
            &server,
            json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "resources/read",
                "params": { "uri": "roke://tasks" }
            }),
        );
        let snap: Value =
            serde_json::from_str(res["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(snap["total"], 1);
        assert_eq!(snap["in_progress"], 1);

        // 7. Domain error returns isError: true
        let err_res = call(
            &server,
            json!({
                "jsonrpc": "2.0",
                "id": 6,
                "method": "tools/call",
                "params": {
                    "name": "get_task",
                    "arguments": { "id": 999 }
                }
            }),
        );
        assert_eq!(err_res["result"]["isError"], true);

        // 8. delete_task via stdio framing
        let input = format!(
            "{}\n",
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "tools/call",
                "params": {
                    "name": "delete_task",
                    "arguments": { "id": 1 }
                }
            })
        );
        let mut out = Vec::new();
        server.serve(Cursor::new(input), &mut out).unwrap();
        let line = String::from_utf8(out).unwrap();
        let del_resp: Value = serde_json::from_str(line.trim()).unwrap();
        assert!(del_resp["result"]["isError"].is_null());

        let _ = fs::remove_dir_all(&dir);
    }
}
