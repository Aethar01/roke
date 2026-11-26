use crate::config::{normalize_device_tag, AppConfig, SyncConfig};
use crate::model::{
    append_tasks_to_archive, archive_path_for, now_millis, parse_due_date, parse_repeat,
    parse_tags, sort_tasks, today_ymd, DueState, FileLock, Priority, SortOrder, Status, Task,
    TaskStore,
};
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
    pub overdue_only: bool,
    pub sort: SortOrder,
}

impl Default for TaskFilter {
    fn default() -> Self {
        Self {
            status: None,
            priority: None,
            tags: Vec::new(),
            tag_match_all: true,
            search: None,
            overdue_only: false,
            sort: SortOrder::Manual,
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
    pub due: Option<String>,
    pub repeat: Option<String>,
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
            due: None,
            repeat: None,
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
    pub due: Option<Option<String>>,
    pub repeat: Option<Option<String>>,
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
            && self.due.is_none()
            && self.repeat.is_none()
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
pub struct GcReport {
    pub pruned_tombstones: usize,
    pub remaining_tombstones: usize,
    pub total_tasks: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveReport {
    pub archived: usize,
    pub archive_path: String,
    pub dry_run: bool,
    pub tasks: Vec<Task>,
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
    if filter.overdue_only {
        let today = today_ymd();
        if task.due_state(&today) != Some(DueState::Overdue) {
            return false;
        }
    }
    true
}

fn acquire_lock(path: &Path) -> Result<FileLock, String> {
    FileLock::acquire(path).map_err(|e| format!("failed to lock {}: {e}", path.display()))
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
    let mut tasks: Vec<Task> = store
        .tasks
        .into_iter()
        .filter(|t| matches_filter(t, filter, &cfg.sync))
        .collect();
    sort_tasks(&mut tasks, filter.sort);
    Ok(tasks)
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
    let _lock = acquire_lock(path)?;
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
            input
                .description
                .replace("\r\n", "\n")
                .trim_end()
                .to_string()
        };
        let tags = normalize_tags(&input.tags);
        let due = match input
            .due
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(raw) => Some(parse_due_date(raw)?),
            None => None,
        };
        let repeat = match input
            .repeat
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(raw) => Some(parse_repeat(raw)?),
            None => None,
        };
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
        if let Some(task) = store.get_mut(id) {
            task.due = due;
            task.repeat = repeat;
        }
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
    let _lock = acquire_lock(path)?;
    let mut store = load_store(path, cfg)?;
    let id = store
        .find_id(selector)
        .ok_or_else(|| format!("task not found: '{selector}'"))?;
    let task = store
        .get_mut(id)
        .ok_or_else(|| format!("task not found: '{selector}'"))?;

    let was_done = task.status == Status::Done;
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
    if let Some(due_update) = input.due {
        task.due = match due_update {
            None => None,
            Some(raw) if raw.trim().is_empty() => None,
            Some(raw) => Some(parse_due_date(&raw)?),
        };
    }
    if let Some(repeat_update) = input.repeat {
        task.repeat = match repeat_update {
            None => None,
            Some(raw) if raw.trim().is_empty() => None,
            Some(raw) => Some(parse_repeat(&raw)?),
        };
    }
    if let Some(status) = input.status {
        if status == Status::Done && !was_done && task.repeat.is_some() {
            task.complete_if_recurring();
        } else {
            task.status = status;
        }
    }

    task.touch();
    let updated = task.clone();
    save_store(&store, path)?;
    Ok(updated)
}

pub fn delete_task(path: &Path, cfg: &AppConfig, selector: &str) -> Result<Task, String> {
    let _lock = acquire_lock(path)?;
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

    // Re-read disk under lock in case another process edited the file while SSH ran
    let _lock = acquire_lock(path)?;
    let latest_disk = load_store(path, cfg)?;
    let merged = merge_stores(
        &latest_disk,
        &outcome.merged_store,
        &local_name,
        &local_name,
    );
    save_store(&merged, path)?;

    Ok(SyncReport {
        synced_peers: outcome.synced_peers,
        total_tasks: merged.tasks.len(),
        errors: outcome.errors,
    })
}

pub fn prune_tombstones(
    path: &Path,
    cfg: &AppConfig,
    max_age_days: u64,
) -> Result<GcReport, String> {
    let _lock = acquire_lock(path)?;
    let mut store = load_store(path, cfg)?;
    let max_age_ms = max_age_days.saturating_mul(24 * 60 * 60 * 1000);
    let pruned = store.prune_tombstones(now_millis(), max_age_ms);
    if pruned > 0 {
        save_store(&store, path)?;
    }
    Ok(GcReport {
        pruned_tombstones: pruned,
        remaining_tombstones: store.tombstones.len(),
        total_tasks: store.tasks.len(),
    })
}

pub fn archive_done(
    path: &Path,
    cfg: &AppConfig,
    older_than_days: Option<u64>,
    dry_run: bool,
) -> Result<ArchiveReport, String> {
    let _lock = acquire_lock(path)?;
    let mut store = load_store(path, cfg)?;
    let cutoff =
        older_than_days.map(|d| now_millis().saturating_sub(d.saturating_mul(24 * 60 * 60 * 1000)));
    let archive_path = archive_path_for(path);

    let target_ids: Vec<u64> = store
        .tasks
        .iter()
        .filter(|t| t.status == Status::Done && cutoff.is_none_or(|c| t.updated_at <= c))
        .map(|t| t.id)
        .collect();

    let tasks: Vec<Task> = target_ids
        .iter()
        .filter_map(|&id| store.get(id).cloned())
        .collect();

    if !dry_run && !tasks.is_empty() {
        append_tasks_to_archive(&archive_path, &tasks)
            .map_err(|e| format!("failed to write archive {}: {e}", archive_path.display()))?;
        for id in &target_ids {
            store.remove(*id);
        }
        save_store(&store, path)?;
    }

    Ok(ArchiveReport {
        archived: tasks.len(),
        archive_path: archive_path.display().to_string(),
        dry_run,
        tasks,
    })
}

pub fn list_archive(path: &Path, cfg: &AppConfig) -> Result<Vec<Task>, String> {
    let archive_path = archive_path_for(path);
    let store = load_store(&archive_path, cfg)?;
    Ok(store.tasks)
}

pub fn unarchive_task(path: &Path, cfg: &AppConfig, selector: &str) -> Result<Task, String> {
    let archive_path = archive_path_for(path);
    let _main_lock = acquire_lock(path)?;
    let _arch_lock = acquire_lock(&archive_path)?;

    let mut archive_store = load_store(&archive_path, cfg)?;
    let arch_id = archive_store
        .find_id(selector)
        .ok_or_else(|| format!("archived task not found: '{selector}'"))?;
    let pos = archive_store
        .tasks
        .iter()
        .position(|t| t.id == arch_id)
        .ok_or_else(|| format!("archived task not found: '{selector}'"))?;
    let mut restored = archive_store.tasks.remove(pos);

    let mut active_store = load_store(path, cfg)?;
    let tomb_ts = active_store.tombstones.remove(&restored.uid).unwrap_or(0);
    restored.updated_at = now_millis()
        .max(restored.updated_at.saturating_add(1))
        .max(tomb_ts.saturating_add(1));
    if restored.id == 0 || active_store.tasks.iter().any(|t| t.id == restored.id) {
        restored.id = active_store.next_id;
        active_store.next_id += 1;
    } else if restored.id >= active_store.next_id {
        active_store.next_id = restored.id + 1;
    }
    let out = restored.clone();
    active_store.tasks.push(restored);

    save_store(&active_store, path)?;
    save_store(&archive_store, &archive_path)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

    fn temp_store_path(name: &str) -> std::path::PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("roke_ops_{name}_{}_{seq}", std::process::id()));
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
                due: Some("2026-12-01".into()),
                repeat: None,
                origin: None,
            },
        )
        .unwrap();
        assert_eq!(t1.id, 1);
        assert_eq!(t1.tags, vec!["agent", "rust"]);
        assert_eq!(t1.due.as_deref(), Some("2026-12-01"));

        let t2 = add_task(
            &path,
            &cfg,
            AddTaskInput {
                title: "Buy coffee".into(),
                description: String::new(),
                status: Status::InProgress,
                priority: Priority::Low,
                tags: vec!["personal".into()],
                due: None,
                repeat: None,
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

    #[test]
    fn concurrent_writes_preserve_all_tasks() {
        let path = temp_store_path("concurrent");
        let cfg = AppConfig::default();

        std::thread::scope(|s| {
            for worker in 0..8 {
                let p = path.clone();
                let c = cfg.clone();
                s.spawn(move || {
                    for i in 0..5 {
                        add_task(
                            &p,
                            &c,
                            AddTaskInput {
                                title: format!("worker-{worker}-task-{i}"),
                                ..AddTaskInput::default()
                            },
                        )
                        .expect("concurrent add_task should succeed");
                    }
                });
            }
        });

        let tasks = list_tasks(&path, &cfg, &TaskFilter::default()).unwrap();
        assert_eq!(tasks.len(), 40);

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn prune_tombstones_respects_age_cutoff() {
        let path = temp_store_path("gc");
        let cfg = AppConfig::default();

        let mut store = TaskStore::new_empty();
        store.add(
            "Keep".into(),
            String::new(),
            Status::Todo,
            Priority::Medium,
            Vec::new(),
        );
        let now = now_millis();
        let old_ts = now.saturating_sub(120 * 24 * 60 * 60 * 1000);
        let fresh_ts = now.saturating_sub(2 * 24 * 60 * 60 * 1000);
        store.tombstones.insert("old-uid".into(), old_ts);
        store.tombstones.insert("fresh-uid".into(), fresh_ts);
        store.save(&path).unwrap();

        let rep = prune_tombstones(&path, &cfg, 90).unwrap();
        assert_eq!(rep.pruned_tombstones, 1);
        assert_eq!(rep.remaining_tombstones, 1);

        let reloaded = TaskStore::load_or_empty(&path).unwrap();
        assert!(!reloaded.tombstones.contains_key("old-uid"));
        assert!(reloaded.tombstones.contains_key("fresh-uid"));

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn archive_done_moves_completed_tasks_and_records_tombstones() {
        let path = temp_store_path("archive");
        let cfg = AppConfig::default();

        add_task(
            &path,
            &cfg,
            AddTaskInput {
                title: "Open task".into(),
                status: Status::Todo,
                ..AddTaskInput::default()
            },
        )
        .unwrap();
        let done = add_task(
            &path,
            &cfg,
            AddTaskInput {
                title: "Finished task".into(),
                status: Status::Done,
                ..AddTaskInput::default()
            },
        )
        .unwrap();

        let dry = archive_done(&path, &cfg, None, true).unwrap();
        assert_eq!(dry.archived, 1);
        assert_eq!(
            list_tasks(&path, &cfg, &TaskFilter::default())
                .unwrap()
                .len(),
            2
        );

        let report = archive_done(&path, &cfg, None, false).unwrap();
        assert_eq!(report.archived, 1);
        assert_eq!(report.tasks[0].uid, done.uid);

        let remaining = TaskStore::load_or_empty(&path).unwrap();
        assert_eq!(remaining.tasks.len(), 1);
        assert!(remaining.tombstones.contains_key(&done.uid));

        let archive_store = TaskStore::load_or_empty(Path::new(&report.archive_path)).unwrap();
        assert_eq!(archive_store.tasks.len(), 1);
        assert_eq!(archive_store.tasks[0].uid, done.uid);

        // Browse and restore from archive
        let archived_list = list_archive(&path, &cfg).unwrap();
        assert_eq!(archived_list.len(), 1);
        assert_eq!(archived_list[0].uid, done.uid);

        let restored = unarchive_task(&path, &cfg, &done.uid).unwrap();
        assert_eq!(restored.uid, done.uid);
        assert!(list_archive(&path, &cfg).unwrap().is_empty());

        let active_after = TaskStore::load_or_empty(&path).unwrap();
        assert_eq!(active_after.tasks.len(), 2);
        assert!(!active_after.tombstones.contains_key(&done.uid));

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn completing_recurring_task_rolls_due_date_and_resets_to_todo() {
        let path = temp_store_path("recurring");
        let cfg = AppConfig::default();

        let task = add_task(
            &path,
            &cfg,
            AddTaskInput {
                title: "Weekly standup prep".into(),
                due: Some("2099-01-01".into()),
                repeat: Some("weekly".into()),
                ..AddTaskInput::default()
            },
        )
        .unwrap();
        assert_eq!(task.repeat.as_deref(), Some("weekly"));

        let updated = update_task(
            &path,
            &cfg,
            &task.uid,
            UpdateTaskInput {
                status: Some(Status::Done),
                ..UpdateTaskInput::default()
            },
        )
        .unwrap();
        assert_eq!(updated.status, Status::Todo);
        assert_eq!(updated.due.as_deref(), Some("2099-01-08"));

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
