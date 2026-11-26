use crate::config::normalize_device_tag;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static UID_SEQ: AtomicU64 = AtomicU64::new(1);
static SAVE_SEQ: AtomicU64 = AtomicU64::new(1);

const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const STALE_LOCK_AGE: Duration = Duration::from_secs(10);

/// Advisory cross-process lockfile guard (`create_new`) with stale-lock recovery.
pub struct FileLock {
    path: PathBuf,
}

impl FileLock {
    pub fn acquire(target: &Path) -> io::Result<Self> {
        if let Some(parent) = target.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let lock_name = format!(
            ".{}.lock",
            target
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("todos.jsonl")
        );
        let lock_path = target.with_file_name(lock_name);
        let deadline = Instant::now() + LOCK_TIMEOUT;

        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Self { path: lock_path });
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    if let Ok(meta) = fs::metadata(&lock_path) {
                        if let Ok(modified) = meta.modified() {
                            if modified
                                .elapsed()
                                .is_ok_and(|age| age >= STALE_LOCK_AGE)
                            {
                                let _ = fs::remove_file(&lock_path);
                                continue;
                            }
                        }
                    }
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("timed out waiting for lock {}", lock_path.display()),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => return Err(err),
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub modified: Option<SystemTime>,
    pub len: u64,
    #[cfg(unix)]
    pub ino: u64,
}

pub fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = fs::metadata(path).ok()?;
    #[cfg(unix)]
    let ino = {
        use std::os::unix::fs::MetadataExt;
        meta.ino()
    };
    Some(FileStamp {
        modified: meta.modified().ok(),
        len: meta.len(),
        #[cfg(unix)]
        ino,
    })
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(1)
}

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => if is_leap_year(y) { 29 } else { 28 },
        _ => 0,
    }
}

/// Convert days since 1970-01-01 (UTC) to `(year, month, day)` using Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

pub fn today_ymd() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

fn offset_ymd(days_offset: i64) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400) + days_offset);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Validate and normalize a due date (`YYYY-MM-DD`, `today`, or `tomorrow`).
pub fn parse_due_date(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("today") {
        return Ok(today_ymd());
    }
    if trimmed.eq_ignore_ascii_case("tomorrow") {
        return Ok(offset_ymd(1));
    }
    let parts: Vec<&str> = trimmed.split('-').collect();
    if parts.len() != 3 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return Err(format!(
            "invalid due date '{trimmed}' (expected YYYY-MM-DD, today, or tomorrow)"
        ));
    }
    let y: i64 = parts[0]
        .parse()
        .map_err(|_| format!("invalid year in due date '{trimmed}'"))?;
    let m: u32 = parts[1]
        .parse()
        .map_err(|_| format!("invalid month in due date '{trimmed}'"))?;
    let d: u32 = parts[2]
        .parse()
        .map_err(|_| format!("invalid day in due date '{trimmed}'"))?;
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) {
        return Err(format!("invalid due date '{trimmed}'"));
    }
    let max_d = days_in_month(y, m);
    if d == 0 || d > max_d {
        return Err(format!("invalid day in due date '{trimmed}'"));
    }
    Ok(format!("{y:04}-{m:02}-{d:02}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DueState {
    Overdue,
    Today,
    Upcoming,
}

pub fn generate_uid(origin: &str) -> String {
    let ts = now_millis();
    let seq = UID_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let slug = normalize_device_tag(origin);
    let prefix = if slug.is_empty() { "local" } else { &slug };
    format!("{prefix}-{ts}-{pid}-{seq}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Status {
    Todo,
    InProgress,
    Done,
}

impl Status {
    pub const ALL: [Status; 3] = [Status::Todo, Status::InProgress, Status::Done];

    pub fn index(self) -> usize {
        match self {
            Status::Todo => 0,
            Status::InProgress => 1,
            Status::Done => 2,
        }
    }

    pub fn from_index(idx: usize) -> Status {
        match idx {
            0 => Status::Todo,
            1 => Status::InProgress,
            _ => Status::Done,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Todo => "To Do",
            Status::InProgress => "In Progress",
            Status::Done => "Done",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Status::Todo => "○",
            Status::InProgress => "◐",
            Status::Done => "✓",
        }
    }

    pub fn next(self) -> Option<Status> {
        match self {
            Status::Todo => Some(Status::InProgress),
            Status::InProgress => Some(Status::Done),
            Status::Done => None,
        }
    }

    pub fn prev(self) -> Option<Status> {
        match self {
            Status::Todo => None,
            Status::InProgress => Some(Status::Todo),
            Status::Done => Some(Status::InProgress),
        }
    }

    pub fn cycle(self) -> Status {
        match self {
            Status::Todo => Status::InProgress,
            Status::InProgress => Status::Done,
            Status::Done => Status::Todo,
        }
    }

    pub fn parse(raw: &str) -> Result<Status, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "todo" | "to-do" | "to_do" | "to do" | "open" => Ok(Status::Todo),
            "in_progress" | "in-progress" | "inprogress" | "in progress" | "doing" | "wip"
            | "active" => Ok(Status::InProgress),
            "done" | "complete" | "completed" | "closed" => Ok(Status::Done),
            other => Err(format!(
                "invalid status '{other}' (expected: todo, in-progress, done)"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Priority {
    Low,
    Medium,
    High,
}

impl Priority {
    pub fn label(self) -> &'static str {
        match self {
            Priority::Low => "Low",
            Priority::Medium => "Medium",
            Priority::High => "High",
        }
    }

    pub fn dot(self) -> &'static str {
        match self {
            Priority::Low => "·",
            Priority::Medium => "•",
            Priority::High => "●",
        }
    }

    pub fn cycle(self) -> Priority {
        match self {
            Priority::Low => Priority::Medium,
            Priority::Medium => Priority::High,
            Priority::High => Priority::Low,
        }
    }

    pub fn parse(raw: &str) -> Result<Priority, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "low" | "l" | "1" => Ok(Priority::Low),
            "medium" | "med" | "m" | "2" => Ok(Priority::Medium),
            "high" | "h" | "3" | "urgent" => Ok(Priority::High),
            other => Err(format!(
                "invalid priority '{other}' (expected: low, medium, high)"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    #[default]
    Manual,
    Priority,
    Due,
    Updated,
}

impl SortOrder {
    pub fn label(self) -> &'static str {
        match self {
            SortOrder::Manual => "manual",
            SortOrder::Priority => "priority",
            SortOrder::Due => "due",
            SortOrder::Updated => "updated",
        }
    }

    pub fn cycle(self) -> Self {
        match self {
            SortOrder::Manual => SortOrder::Priority,
            SortOrder::Priority => SortOrder::Due,
            SortOrder::Due => SortOrder::Updated,
            SortOrder::Updated => SortOrder::Manual,
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "manual" | "order" | "default" => Ok(SortOrder::Manual),
            "priority" | "prio" | "p" => Ok(SortOrder::Priority),
            "due" | "date" | "deadline" => Ok(SortOrder::Due),
            "updated" | "recent" => Ok(SortOrder::Updated),
            other => Err(format!(
                "invalid sort '{other}' (expected: manual, priority, due, updated)"
            )),
        }
    }
}

fn priority_rank(p: Priority) -> u8 {
    match p {
        Priority::High => 0,
        Priority::Medium => 1,
        Priority::Low => 2,
    }
}

pub fn sort_task_refs(tasks: &mut [&Task], sort: SortOrder) {
    match sort {
        SortOrder::Manual => {}
        SortOrder::Priority => {
            tasks.sort_by(|a, b| {
                priority_rank(a.priority)
                    .cmp(&priority_rank(b.priority))
                    .then_with(|| match (&a.due, &b.due) {
                        (Some(da), Some(db)) => da.cmp(db),
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (None, None) => std::cmp::Ordering::Equal,
                    })
            });
        }
        SortOrder::Due => {
            tasks.sort_by(|a, b| match (&a.due, &b.due) {
                (Some(da), Some(db)) => da
                    .cmp(db)
                    .then_with(|| priority_rank(a.priority).cmp(&priority_rank(b.priority))),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => priority_rank(a.priority).cmp(&priority_rank(b.priority)),
            });
        }
        SortOrder::Updated => {
            tasks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        }
    }
}

pub fn sort_tasks(tasks: &mut [Task], sort: SortOrder) {
    match sort {
        SortOrder::Manual => {}
        SortOrder::Priority => {
            tasks.sort_by(|a, b| {
                priority_rank(a.priority)
                    .cmp(&priority_rank(b.priority))
                    .then_with(|| match (&a.due, &b.due) {
                        (Some(da), Some(db)) => da.cmp(db),
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (None, None) => std::cmp::Ordering::Equal,
                    })
            });
        }
        SortOrder::Due => {
            tasks.sort_by(|a, b| match (&a.due, &b.due) {
                (Some(da), Some(db)) => da
                    .cmp(db)
                    .then_with(|| priority_rank(a.priority).cmp(&priority_rank(b.priority))),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => priority_rank(a.priority).cmp(&priority_rank(b.priority)),
            });
        }
        SortOrder::Updated => {
            tasks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub uid: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default = "default_status")]
    pub status: Status,
    #[serde(default = "default_priority")]
    pub priority: Priority,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
}

fn default_status() -> Status {
    Status::Todo
}

#[derive(Debug, Serialize, Deserialize)]
struct TombstoneRecord {
    uid: String,
    deleted_at: u64,
}

impl Task {
    pub fn touch(&mut self) {
        let now = now_millis();
        self.updated_at = if now <= self.updated_at {
            self.updated_at + 1
        } else {
            now
        };
    }

    pub fn due_state(&self, today: &str) -> Option<DueState> {
        let due = self.due.as_deref()?;
        if self.status == Status::Done {
            return Some(DueState::Upcoming);
        }
        match due.cmp(today) {
            std::cmp::Ordering::Less => Some(DueState::Overdue),
            std::cmp::Ordering::Equal => Some(DueState::Today),
            std::cmp::Ordering::Greater => Some(DueState::Upcoming),
        }
    }

    /// Count Markdown checklist items (`- [ ]` / `- [x]`) in `description`.
    pub fn checklist_progress(&self) -> Option<(usize, usize)> {
        let mut done = 0usize;
        let mut total = 0usize;
        for line in self.description.lines() {
            let trimmed = line.trim_start();
            let rest = trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))
                .or_else(|| trimmed.strip_prefix("+ "));
            if let Some(after_bullet) = rest {
                let after_bullet = after_bullet.trim_start();
                if after_bullet.starts_with("[ ]") {
                    total += 1;
                } else if after_bullet.starts_with("[x]") || after_bullet.starts_with("[X]") {
                    total += 1;
                    done += 1;
                }
            }
        }
        (total > 0).then_some((done, total))
    }
}

pub fn parse_tags(input: &str) -> Vec<String> {
    let mut tags = Vec::new();
    for part in input.split(|c: char| c == ',' || c.is_whitespace()) {
        let cleaned = part.trim().trim_start_matches('#').to_lowercase();
        if !cleaned.is_empty() && !tags.contains(&cleaned) {
            tags.push(cleaned);
        }
    }
    tags
}

pub fn format_tags(tags: &[String]) -> String {
    tags.join(", ")
}

fn default_priority() -> Priority {
    Priority::Medium
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStore {
    pub next_id: u64,
    pub tasks: Vec<Task>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tombstones: HashMap<String, u64>,
}

impl Default for TaskStore {
    fn default() -> Self {
        Self::new_empty()
    }
}

impl TaskStore {
    pub fn new_empty() -> Self {
        Self {
            next_id: 1,
            tasks: Vec::new(),
            tombstones: HashMap::new(),
        }
    }

    pub fn with_starter_tasks() -> Self {
        let mut store = Self::new_empty();
        store.add(
            "Switch views with Tab or click Todos / Kanban".into(),
            "Press 1 for Todo List, 2 for Kanban Board, or Tab to toggle.".into(),
            Status::Todo,
            Priority::High,
            Vec::new(),
        );
        store.add(
            "Move cards with H/L or drag with mouse".into(),
            "In Kanban view, drag any card across columns or press Shift+H / Shift+L.".into(),
            Status::InProgress,
            Priority::Medium,
            Vec::new(),
        );
        store.add(
            "Press 'a' to add a task, '?' for all shortcuts".into(),
            "Use j/k/h/l, gg/G, Space to toggle, d to delete, and u to undo.".into(),
            Status::Done,
            Priority::Low,
            Vec::new(),
        );
        store
    }

    pub fn add(
        &mut self,
        title: String,
        description: String,
        status: Status,
        priority: Priority,
        tags: Vec<String>,
    ) -> u64 {
        self.add_with_origin(title, description, status, priority, tags, "local")
    }

    pub fn add_with_origin(
        &mut self,
        title: String,
        description: String,
        status: Status,
        priority: Priority,
        tags: Vec<String>,
        origin: &str,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let slug = normalize_device_tag(origin);
        let origin_opt = if slug.is_empty() { None } else { Some(slug) };
        let uid = generate_uid(origin);
        let next_order = self
            .tasks
            .iter()
            .filter_map(|t| t.order)
            .max()
            .map(|m| m.saturating_add(1000));
        let now = now_millis();
        self.tasks.push(Task {
            id,
            uid,
            title,
            description,
            status,
            priority,
            tags,
            due: None,
            created_at: Some(now),
            updated_at: now,
            origin: origin_opt,
            order: next_order,
        });
        id
    }

    pub fn ensure_orders(&mut self) {
        if self.tasks.iter().all(|t| t.order.is_some()) {
            return;
        }
        for (idx, task) in self.tasks.iter_mut().enumerate() {
            if task.order.is_none() {
                task.order = Some(((idx + 1) as i64) * 1000);
            }
        }
    }

    pub fn swap_order(&mut self, pos_a: usize, pos_b: usize) {
        if pos_a == pos_b || pos_a >= self.tasks.len() || pos_b >= self.tasks.len() {
            return;
        }
        self.ensure_orders();
        let ord_a = self.tasks[pos_a].order;
        let ord_b = self.tasks[pos_b].order;
        self.tasks.swap(pos_a, pos_b);
        self.tasks[pos_a].order = ord_a;
        self.tasks[pos_b].order = ord_b;
        self.tasks[pos_a].touch();
        self.tasks[pos_b].touch();
    }

    pub fn move_to_position(&mut self, from_pos: usize, to_pos: usize) {
        if from_pos == to_pos || from_pos >= self.tasks.len() || to_pos >= self.tasks.len() {
            return;
        }
        self.ensure_orders();
        let mut task = self.tasks.remove(from_pos);
        self.tasks.insert(to_pos, task.clone());
        let prev_ord = to_pos
            .checked_sub(1)
            .and_then(|i| self.tasks.get(i))
            .and_then(|t| t.order);
        let next_ord = self.tasks.get(to_pos + 1).and_then(|t| t.order);
        let new_order = match (prev_ord, next_ord) {
            (Some(a), Some(b)) if b - a > 1 => Some(a + (b - a) / 2),
            (None, Some(b)) => Some(b.saturating_sub(1000)),
            (Some(a), None) => Some(a.saturating_add(1000)),
            _ => None,
        };
        if let Some(ord) = new_order {
            task.order = Some(ord);
            task.touch();
            self.tasks[to_pos] = task;
        } else {
            // Rebalance orders cleanly when adjacent slots collided
            for (idx, t) in self.tasks.iter_mut().enumerate() {
                let target = ((idx + 1) as i64) * 1000;
                if t.order != Some(target) {
                    t.order = Some(target);
                    t.touch();
                }
            }
        }
    }

    pub fn ensure_sync_metadata(&mut self, default_origin: &str) {
        let fallback = normalize_device_tag(default_origin);
        let fallback_opt = if fallback.is_empty() {
            None
        } else {
            Some(fallback)
        };
        for task in &mut self.tasks {
            if task.uid.trim().is_empty() {
                task.uid = generate_uid(default_origin);
            }
            if task.updated_at == 0 {
                task.updated_at = now_millis();
            }
            if task.origin.as_deref().unwrap_or("").is_empty() {
                task.origin = fallback_opt.clone();
            }
        }
    }

    #[allow(dead_code)]
    pub fn all_tags(&self) -> Vec<String> {
        let mut tags: Vec<String> = self
            .tasks
            .iter()
            .flat_map(|t| t.tags.iter().cloned())
            .collect();
        tags.sort();
        tags.dedup();
        tags
    }

    pub fn get(&self, id: u64) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    /// Resolve a task selector by exact UID, numeric ID, or unique UID prefix (>= 4 chars).
    pub fn find_id(&self, selector: &str) -> Option<u64> {
        let needle = selector.trim();
        if needle.is_empty() {
            return None;
        }
        if let Some(task) = self.tasks.iter().find(|t| t.uid == needle) {
            return Some(task.id);
        }
        if let Ok(id) = needle.parse::<u64>() {
            if self.tasks.iter().any(|t| t.id == id) {
                return Some(id);
            }
        }
        if needle.len() >= 4 {
            let mut matches = self.tasks.iter().filter(|t| t.uid.starts_with(needle));
            if let (Some(first), None) = (matches.next(), matches.next()) {
                return Some(first.id);
            }
        }
        None
    }

    pub fn remove(&mut self, id: u64) -> Option<Task> {
        let pos = self.tasks.iter().position(|t| t.id == id)?;
        let removed = self.tasks.remove(pos);
        if !removed.uid.is_empty() {
            let deleted_at = now_millis().max(removed.updated_at + 1);
            self.tombstones.insert(removed.uid.clone(), deleted_at);
        }
        Some(removed)
    }

    /// Remove tombstones older than `max_age_ms` relative to `now_ms`.
    /// Returns the number of pruned tombstones.
    pub fn prune_tombstones(&mut self, now_ms: u64, max_age_ms: u64) -> usize {
        let cutoff = now_ms.saturating_sub(max_age_ms);
        let before = self.tombstones.len();
        self.tombstones.retain(|_, &mut ts| ts > cutoff);
        before - self.tombstones.len()
    }

    pub fn from_str_auto(raw: &str) -> io::Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(Self::new_empty());
        }

        // Legacy single-object JSON format (`{"next_id": ..., "tasks": [...]}`)
        if let Ok(mut legacy) = serde_json::from_str::<TaskStore>(trimmed) {
            let max_id = legacy.tasks.iter().map(|t| t.id).max().unwrap_or(0);
            if legacy.next_id <= max_id {
                legacy.next_id = max_id + 1;
            }
            return Ok(legacy);
        }

        Self::from_jsonl(trimmed)
    }

    pub fn from_jsonl(raw: &str) -> io::Result<Self> {
        let mut tasks: Vec<Task> = Vec::new();
        let mut tombstones: HashMap<String, u64> = HashMap::new();
        let mut parsed_any = false;
        let mut first_err: Option<String> = None;

        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            if let Ok(tomb) = serde_json::from_str::<TombstoneRecord>(line) {
                if !tomb.uid.is_empty() {
                    tombstones
                        .entry(tomb.uid)
                        .and_modify(|cur| {
                            if tomb.deleted_at > *cur {
                                *cur = tomb.deleted_at;
                            }
                        })
                        .or_insert(tomb.deleted_at);
                    parsed_any = true;
                    continue;
                }
            }

            match serde_json::from_str::<Task>(line) {
                Ok(task) => {
                    parsed_any = true;
                    if !task.uid.is_empty() {
                        if let Some(pos) = tasks.iter().position(|t| t.uid == task.uid) {
                            if task.updated_at >= tasks[pos].updated_at {
                                tasks[pos] = task;
                            }
                            continue;
                        }
                    }
                    tasks.push(task);
                }
                Err(err) => {
                    if first_err.is_none() {
                        first_err = Some(err.to_string());
                    }
                }
            }
        }

        if !parsed_any {
            if let Some(err) = first_err {
                return Err(io::Error::new(io::ErrorKind::InvalidData, err));
            }
        }

        // Remove tasks superseded by tombstones and ensure unique non-zero IDs
        tasks.retain(|t| {
            t.uid.is_empty()
                || tombstones
                    .get(&t.uid)
                    .is_none_or(|&del_at| del_at < t.updated_at)
        });

        let mut next_id = tasks.iter().map(|t| t.id).max().unwrap_or(0) + 1;
        let mut seen_ids = std::collections::HashSet::new();
        for task in &mut tasks {
            if task.id == 0 || !seen_ids.insert(task.id) {
                task.id = next_id;
                next_id += 1;
                seen_ids.insert(task.id);
            }
        }

        Ok(Self {
            next_id,
            tasks,
            tombstones,
        })
    }

    pub fn to_jsonl(&self) -> io::Result<String> {
        let mut out = String::new();
        for task in &self.tasks {
            let line = serde_json::to_string(task).map_err(io::Error::other)?;
            out.push_str(&line);
            out.push('\n');
        }
        let mut tombs: Vec<(&String, &u64)> = self.tombstones.iter().collect();
        tombs.sort_by(|a, b| a.0.cmp(b.0));
        for (uid, &deleted_at) in tombs {
            let rec = TombstoneRecord {
                uid: uid.clone(),
                deleted_at,
            };
            let line = serde_json::to_string(&rec).map_err(io::Error::other)?;
            out.push_str(&line);
            out.push('\n');
        }
        Ok(out)
    }

    pub fn load_or_init(path: &Path) -> io::Result<Self> {
        if !path.exists() {
            // Auto-migrate legacy todos.json in the same directory if present
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                let legacy_path = path.with_extension("json");
                if legacy_path.exists() {
                    let raw = fs::read_to_string(&legacy_path)?;
                    let store = Self::from_str_auto(&raw)?;
                    let _ = store.save(path);
                    return Ok(store);
                }
            }
            let store = Self::with_starter_tasks();
            let _ = store.save(path);
            return Ok(store);
        }
        let raw = fs::read_to_string(path)?;
        Self::from_str_auto(&raw)
    }

    /// Load an existing task store (migrating legacy `.json` if present) or return an empty store
    /// without injecting tutorial starter tasks. Ideal for CLI, MCP, and live reloads.
    pub fn load_or_empty(path: &Path) -> io::Result<Self> {
        if !path.exists() {
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                let legacy_path = path.with_extension("json");
                if legacy_path.exists() {
                    let raw = fs::read_to_string(&legacy_path)?;
                    return Self::from_str_auto(&raw);
                }
            }
            return Ok(Self::new_empty());
        }
        let raw = fs::read_to_string(path)?;
        Self::from_str_auto(&raw)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let content = self.to_jsonl()?;
        let seq = SAVE_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_name = format!(
            ".{}.tmp.{}.{}",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("todos.jsonl"),
            std::process::id(),
            seq
        );
        let tmp_path = path.with_file_name(tmp_name);
        fs::write(&tmp_path, content)?;
        fs::rename(&tmp_path, path)
    }
}

pub fn default_storage_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.trim().is_empty() {
            return PathBuf::from(xdg).join("roke").join("todos.jsonl");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("roke")
                .join("todos.jsonl");
        }
    }
    PathBuf::from("todos.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_navigation_and_cycle() {
        assert_eq!(Status::Todo.prev(), None);
        assert_eq!(Status::Todo.next(), Some(Status::InProgress));
        assert_eq!(Status::InProgress.next(), Some(Status::Done));
        assert_eq!(Status::Done.next(), None);
        assert_eq!(Status::Done.prev(), Some(Status::InProgress));
        assert_eq!(Status::Done.cycle(), Status::Todo);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("roke_test_{}", std::process::id()));
        let path = dir.join("todos.jsonl");
        let _ = fs::remove_dir_all(&dir);

        let mut store = TaskStore::new_empty();
        let id1 = store.add(
            "First".into(),
            "Details".into(),
            Status::Todo,
            Priority::High,
            parse_tags("#Work, personal, work"),
        );
        let id2 = store.add(
            "Second".into(),
            String::new(),
            Status::Done,
            Priority::Low,
            Vec::new(),
        );
        let id3 = store.add(
            "Deleted".into(),
            String::new(),
            Status::Todo,
            Priority::Low,
            Vec::new(),
        );
        let del_uid = store.get(id3).unwrap().uid.clone();
        store.remove(id3);
        store.save(&path).expect("save should succeed");

        let raw = fs::read_to_string(&path).unwrap();
        // 2 task lines + 1 tombstone line = 3 JSONL lines
        assert_eq!(raw.lines().count(), 3);

        let loaded = TaskStore::load_or_init(&path).expect("load should succeed");
        assert_eq!(loaded.tasks.len(), 2);
        assert_eq!(loaded.get(id1).unwrap().title, "First");
        assert_eq!(loaded.get(id1).unwrap().tags, vec!["work", "personal"]);
        assert_eq!(loaded.all_tags(), vec!["personal", "work"]);
        assert_eq!(loaded.get(id2).unwrap().status, Status::Done);
        assert!(loaded.tombstones.contains_key(&del_uid));

        // Hand-appended minimal JSONL line works cleanly
        let appended = format!("{raw}{{\"title\":\"Quick append\"}}\n");
        let parsed = TaskStore::from_str_auto(&appended).unwrap();
        assert_eq!(parsed.tasks.len(), 3);
        assert_eq!(parsed.tasks[2].title, "Quick append");
        assert_eq!(parsed.tasks[2].status, Status::Todo);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn due_date_parsing_and_state() {
        assert_eq!(parse_due_date("2026-02-28").unwrap(), "2026-02-28");
        assert_eq!(parse_due_date("2024-02-29").unwrap(), "2024-02-29");
        assert!(parse_due_date("2026-02-29").is_err());
        assert!(parse_due_date("2026-13-01").is_err());
        assert!(parse_due_date("not-a-date").is_err());
        assert_eq!(parse_due_date("today").unwrap().len(), 10);
        assert_eq!(parse_due_date("tomorrow").unwrap().len(), 10);

        let mut store = TaskStore::new_empty();
        let id = store.add("Tax".into(), String::new(), Status::Todo, Priority::High, Vec::new());
        store.get_mut(id).unwrap().due = Some("2026-01-01".into());
        assert_eq!(store.get(id).unwrap().due_state("2026-06-01"), Some(DueState::Overdue));
        assert_eq!(store.get(id).unwrap().due_state("2026-01-01"), Some(DueState::Today));
        assert_eq!(store.get(id).unwrap().due_state("2025-12-31"), Some(DueState::Upcoming));
    }
}
