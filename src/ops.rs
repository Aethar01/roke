use crate::config::{normalize_device_tag, AppConfig, SyncConfig};
use crate::model::{parse_tags, Priority, Status, Task, TaskStore};
use crate::sync::{merge_stores, sync_all_peers};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct TaskFilter {
    pub status: Option<Status>,
    pub priority: Option<Priority>,
    pub tags: Vec<String>,
    pub tag_match_all: bool,
    pub search: Option<String>,
}

impl Default for TaskFilter {
    fn default() -> Self {
        Self {
            status: None,
            priority: None,
            tags: Vec::new(),
            tag_match_all: true,
            search: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AddTaskInput {
    pub title: String,
    pub description: String,
    pub status: Status,
    pub priority: Priority,
    pub tags: Vec<String>,
    pub origin: Option<String>,
}

impl Default for AddTaskInput {
    fn default() -> Self {
        Self {
            title: String::new(),
            description: String::new(),
            status: Status::Todo,
            priority: Priority::Medium,
            tags: Vec::new(),
            origin: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct UpdateTaskInput {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<Status>,
    pub priority: Option<Priority>,
    pub tags: Option<Vec<String>>,
    pub add_tags: Vec<String>,
    pub remove_tags: Vec<String>,
}

impl UpdateTaskInput {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.description.is_none()
            && self.status.is_none()
            && self.priority.is_none()
            && self.tags.is_none()
            && self.add_tags.is_empty()
            && self.remove_tags.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagCount {
    pub tag: String,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncReport {
    pub synced_peers: usize,
    pub total_tasks: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardSnapshot {
    pub total: usize,
    pub todo: usize,
    pub in_progress: usize,
    pub done: usize,
    pub tasks: Vec<Task>,
}

pub fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for raw in tags {
        for tag in parse_tags(raw) {
            if !out.contains(&tag) {
                out.push(tag);
            }
        }
    }
    out
}

pub fn effective_tags_for(task: &Task, sync_cfg: &SyncConfig) -> Vec<String> {
    let mut tags = task.tags.clone();
    if let Some(origin) = &task.origin {
        if sync_cfg.should_inject_tag_for(origin) {
            let slug = normalize_device_tag(origin);
            if !slug.is_empty() && !tags.iter().any(|t| t == &slug) {
                tags.push(slug);
            }
        }
    }
    tags
}

fn matches_filter(task: &Task, filter: &TaskFilter, sync_cfg: &SyncConfig) -> bool {
    if let Some(status) = filter.status {
        if task.status != status {
            return false;
        }
    }
    if let Some(priority) = filter.priority {
        if task.priority != priority {
            return false;
        }
    }
    let eff_tags = effective_tags_for(task, sync_cfg);
    if !filter.tags.is_empty() {
        let wanted = normalize_tags(&filter.tags);
        if !wanted.is_empty() {
            let ok = if filter.tag_match_all {
                wanted.iter().all(|f| eff_tags.iter().any(|t| t == f))
            } else {
                wanted.iter().any(|f| eff_tags.iter().any(|t| t == f))
            };
            if !ok {
                return false;
            }
        }
    }
    if let Some(query) = &filter.search {
        let q = query.trim().to_lowercase();
        if !q.is_empty() {
            let tag_q = q.trim_start_matches('#');
            let matched = task.title.to_lowercase().contains(&q)
                || task.description.to_lowercase().contains(&q)
                || (!tag_q.is_empty() && eff_tags.iter().any(|t| t.contains(tag_q)));
            if !matched {
                return false;
            }
        }
    }
    true
}

fn load_store(path: &Path, cfg: &AppConfig) -> Result<TaskStore, String> {
    let mut store = TaskStore::load_or_empty(path)
        .map_err(|e| format!("failed to load {}: {e}", path.display()))?;
    store.ensure_sync_metadata(&cfg.sync.effective_device_name());
    Ok(store)
}

fn save_store(store: &TaskStore, path: &Path) -> Result<(), String> {
    store
        .save(path)
        .map_err(|e| format!("failed to save {}: {e}", path.display()))
}

pub fn list_tasks(path: &Path, cfg: &AppConfig, filter: &TaskFilter) -> Result<Vec<Task>, String> {
    let store = load_store(path, cfg)?;
    Ok(store
        .tasks
        .into_iter()
        .filter(|t| matches_filter(t, filter, &cfg.sync))
        .collect())
}

pub fn board_snapshot(
    path: &Path,
    cfg: &AppConfig,
    filter: &TaskFilter,
) -> Result<BoardSnapshot, String> {
    let tasks = list_tasks(path, cfg, filter)?;
    let mut todo = 0;
    let mut in_progress = 0;
    let mut done = 0;
    for task in &tasks {
        match task.status {
            Status::Todo => todo += 1,
            Status::InProgress => in_progress += 1,
            Status::Done => done += 1,
        }
    }
    Ok(BoardSnapshot {
        total: tasks.len(),
        todo,
        in_progress,
        done,
        tasks,
    })
}

pub fn get_task(path: &Path, cfg: &AppConfig, selector: &str) -> Result<Task, String> {
    let store = load_store(path, cfg)?;
    let id = store
        .find_id(selector)
        .ok_or_else(|| format!("task not found: '{selector}'"))?;
    store
        .get(id)
        .cloned()
        .ok_or_else(|| format!("task not found: '{selector}'"))
}

pub fn add_task(path: &Path, cfg: &AppConfig, input: AddTaskInput) -> Result<Task, String> {
    let mut created = add_tasks_batch(path, cfg, vec![input])?;
    Ok(created.remove(0))
}

pub fn add_tasks_batch(
    path: &Path,
    cfg: &AppConfig,
    inputs: Vec<AddTaskInput>,
) -> Result<Vec<Task>, String> {
    if inputs.is_empty() {
        return Err("no tasks provided".into());
    }
    let mut store = load_store(path, cfg)?;
    let default_origin = cfg.sync.effective_device_name();
    let mut created_ids = Vec::with_capacity(inputs.len());

    for input in inputs {
        let title = input.title.trim().to_string();
        if title.is_empty() {
            return Err("task title cannot be empty".into());
        }
        let description = if input.description.trim().is_empty() {
            String::new()
        } else {
            input.description.replace("\r\n", "\n").trim_end().to_string()
        };
        let tags = normalize_tags(&input.tags);
        let origin = input
            .origin
            .as_deref()
            .map(normalize_device_tag)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default_origin.clone());
        let id = store.add_with_origin(
            title,
            description,
            input.status,
            input.priority,
            tags,
            &origin,
        );
        created_ids.push(id);
    }

    save_store(&store, path)?;
    let created = created_ids
        .into_iter()
        .filter_map(|id| store.get(id).cloned())
        .collect();
    Ok(created)
}

pub fn update_task(
    path: &Path,
    cfg: &AppConfig,
    selector: &str,
    input: UpdateTaskInput,
) -> Result<Task, String> {
    if input.is_empty() {
        return Err("no fields specified to update".into());
    }
    let mut store = load_store(path, cfg)?;
    let id = store
        .find_id(selector)
        .ok_or_else(|| format!("task not found: '{selector}'"))?;
    let task = store
        .get_mut(id)
        .ok_or_else(|| format!("task not found: '{selector}'"))?;

    if let Some(title) = input.title {
        let trimmed = title.trim().to_string();
        if trimmed.is_empty() {
            return Err("task title cannot be empty".into());
        }
        task.title = trimmed;
    }
    if let Some(desc) = input.description {
        task.description = if desc.trim().is_empty() {
            String::new()
        } else {
            desc.replace("\r\n", "\n").trim_end().to_string()
        };
    }
    if let Some(status) = input.status {
        task.status = status;
    }
    if let Some(priority) = input.priority {
        task.priority = priority;
    }
    if let Some(tags) = input.tags {
        task.tags = normalize_tags(&tags);
    }
    if !input.add_tags.is_empty() {
        for tag in normalize_tags(&input.add_tags) {
            if !task.tags.contains(&tag) {
                task.tags.push(tag);
            }
        }
    }
    if !input.remove_tags.is_empty() {
        let to_remove = normalize_tags(&input.remove_tags);
        task.tags.retain(|t| !to_remove.contains(t));
    }

    task.touch();
    let updated = task.clone();
    save_store(&store, path)?;
    Ok(updated)
}

pub fn delete_task(path: &Path, cfg: &AppConfig, selector: &str) -> Result<Task, String> {
    let mut store = load_store(path, cfg)?;
    let id = store
        .find_id(selector)
        .ok_or_else(|| format!("task not found: '{selector}'"))?;
    let removed = store
        .remove(id)
        .ok_or_else(|| format!("task not found: '{selector}'"))?;
    save_store(&store, path)?;
    Ok(removed)
}

pub fn list_tags(path: &Path, cfg: &AppConfig) -> Result<Vec<TagCount>, String> {
    let store = load_store(path, cfg)?;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for task in &store.tasks {
        for tag in effective_tags_for(task, &cfg.sync) {
            *counts.entry(tag).or_insert(0) += 1;
        }
    }
    Ok(counts
        .into_iter()
        .map(|(tag, count)| TagCount { tag, count })
        .collect())
}

pub fn sync_tasks(path: &Path, cfg: &AppConfig) -> Result<SyncReport, String> {
    if cfg.sync.peers.is_empty() {
        return Err("no SSH peers configured in config.toml".into());
    }
    let store = load_store(path, cfg)?;
    let local_name = cfg.sync.effective_device_name();
    let outcome = sync_all_peers(store, cfg.sync.clone());

    // Re-read disk in case another process edited the file while SSH ran
    let latest_disk = load_store(path, cfg)?;
    let merged = merge_stores(&latest_disk, &outcome.merged_store, &local_name, &local_name);
    save_store(&merged, path)?;

    Ok(SyncReport {
        synced_peers: outcome.synced_peers,
        total_tasks: merged.tasks.len(),
        errors: outcome.errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

    fn temp_store_path(name: &str) -> std::path::PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "roke_ops_{name}_{}_{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir.join("todos.jsonl")
    }

    #[test]
    fn crud_and_filtering_work_by_id_and_uid() {
        let path = temp_store_path("crud");
        let cfg = AppConfig::default();

        let t1 = add_task(
            &path,
            &cfg,
            AddTaskInput {
                title: "Write MCP server".into(),
                description: "Expose JSON-RPC tools over stdio".into(),
                status: Status::Todo,
                priority: Priority::High,
                tags: vec!["#agent".into(), "rust".into()],
                origin: None,
            },
        )
        .unwrap();
        assert_eq!(t1.id, 1);
        assert_eq!(t1.tags, vec!["agent", "rust"]);

        let t2 = add_task(
            &path,
            &cfg,
            AddTaskInput {
                title: "Buy coffee".into(),
                description: String::new(),
                status: Status::InProgress,
                priority: Priority::Low,
                tags: vec!["personal".into()],
                origin: None,
            },
        )
        .unwrap();
        assert_eq!(t2.id, 2);

        // Filter by tag + status
        let filtered = list_tasks(
            &path,
            &cfg,
            &TaskFilter {
                status: Some(Status::Todo),
                tags: vec!["rust".into()],
                ..TaskFilter::default()
            },
        )
        .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].uid, t1.uid);

        // Update by UID with incremental add/remove tags
        let updated = update_task(
            &path,
            &cfg,
            &t1.uid,
            UpdateTaskInput {
                status: Some(Status::Done),
                add_tags: vec!["shipped".into()],
                remove_tags: vec!["agent".into()],
                ..UpdateTaskInput::default()
            },
        )
        .unwrap();
        assert_eq!(updated.status, Status::Done);
        assert_eq!(updated.tags, vec!["rust", "shipped"]);
        assert!(updated.updated_at > t1.updated_at);

        // List tags with counts
        let tags = list_tags(&path, &cfg).unwrap();
        assert_eq!(
            tags,
            vec![
                TagCount {
                    tag: "personal".into(),
                    count: 1
                },
                TagCount {
                    tag: "rust".into(),
                    count: 1
                },
                TagCount {
                    tag: "shipped".into(),
                    count: 1
                },
            ]
        );

        // Delete by numeric ID records a tombstone
        let removed = delete_task(&path, &cfg, "2").unwrap();
        assert_eq!(removed.uid, t2.uid);

        let raw_store = TaskStore::load_or_empty(&path).unwrap();
        assert_eq!(raw_store.tasks.len(), 1);
        assert!(raw_store.tombstones.contains_key(&t2.uid));

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
