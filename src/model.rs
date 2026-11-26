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
                            if modified.elapsed().is_ok_and(|age| age >= STALE_LOCK_AGE) {
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
        2 => {
            if is_leap_year(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Convert `(year, month, day)` to days since 1970-01-01 using Howard Hinnant's algorithm.
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = m as u64;
    let d = d as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + (doe as i64) - 719_468
}

/// Convert days since 1970-01-01 to `(year, month, day)` using Howard Hinnant's algorithm.
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
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

#[cfg(unix)]
fn local_today_ymd_tuple() -> (i64, u32, u32) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ptr = unsafe { libc::localtime_r(&secs, &mut tm) };
    if ptr.is_null() {
        #[allow(clippy::unnecessary_cast)]
        return civil_from_days((secs as i64).div_euclid(86_400));
    }
    (
        (tm.tm_year as i64) + 1900,
        (tm.tm_mon as u32) + 1,
        tm.tm_mday as u32,
    )
}

#[cfg(not(unix))]
fn local_today_ymd_tuple() -> (i64, u32, u32) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    civil_from_days(secs.div_euclid(86_400))
}

pub fn today_ymd() -> String {
    let (y, m, d) = local_today_ymd_tuple();
    format!("{y:04}-{m:02}-{d:02}")
}

fn add_months((y, m, d): (i64, u32, u32), months: i64) -> (i64, u32, u32) {
    let total = (y * 12 + (m as i64 - 1)) + months;
    let ny = total.div_euclid(12);
    let nm = (total.rem_euclid(12) + 1) as u32;
    let nd = d.min(days_in_month(ny, nm));
    (ny, nm, nd)
}

fn parse_ymd_tuple(ymd: &str) -> Option<(i64, u32, u32)> {
    let parts: Vec<&str> = ymd.split('-').collect();
    if parts.len() != 3 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return None;
    }
    let y: i64 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2].parse().ok()?;
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) {
        return None;
    }
    if d == 0 || d > days_in_month(y, m) {
        return None;
    }
    Some((y, m, d))
}

/// Weekday for days since 1970-01-01 (Thursday): 0 = Sun, 1 = Mon, ..., 6 = Sat.
fn weekday_from_days(days: i64) -> i64 {
    (days + 4).rem_euclid(7)
}

fn parse_weekday_name(s: &str) -> Option<i64> {
    match s {
        "sun" | "sunday" => Some(0),
        "mon" | "monday" => Some(1),
        "tue" | "tues" | "tuesday" => Some(2),
        "wed" | "wednesday" => Some(3),
        "thu" | "thur" | "thurs" | "thursday" => Some(4),
        "fri" | "friday" => Some(5),
        "sat" | "saturday" => Some(6),
        _ => None,
    }
}

/// Validate and normalize a due date (`YYYY-MM-DD`, `today`, `tomorrow`, `+3d`, `+2w`, `+1m`, `mon`..`sun`, `eow`, `eom`).
pub fn parse_due_date(raw: &str) -> Result<String, String> {
    parse_due_date_from(raw, local_today_ymd_tuple())
}

pub(crate) fn parse_due_date_from(raw: &str, base: (i64, u32, u32)) -> Result<String, String> {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    let base_days = days_from_civil(base.0, base.1, base.2);

    if lower == "today" || lower == "tod" {
        return Ok(format!("{:04}-{:02}-{:02}", base.0, base.1, base.2));
    }
    if lower == "tomorrow" || lower == "tom" {
        let (ny, nm, nd) = civil_from_days(base_days + 1);
        return Ok(format!("{ny:04}-{nm:02}-{nd:02}"));
    }
    if lower == "eow" {
        // Friday of this week (or next Friday if Sat/Sun)
        let wd = weekday_from_days(base_days);
        let delta = if wd <= 5 { 5 - wd } else { 6 };
        let (ny, nm, nd) = civil_from_days(base_days + delta);
        return Ok(format!("{ny:04}-{nm:02}-{nd:02}"));
    }
    if lower == "eom" {
        let last_d = days_in_month(base.0, base.1);
        return Ok(format!("{:04}-{:02}-{last_d:02}", base.0, base.1));
    }
    if let Some(target_wd) = parse_weekday_name(&lower) {
        let cur_wd = weekday_from_days(base_days);
        let mut delta = (target_wd - cur_wd).rem_euclid(7);
        if delta == 0 {
            delta = 7;
        }
        let (ny, nm, nd) = civil_from_days(base_days + delta);
        return Ok(format!("{ny:04}-{nm:02}-{nd:02}"));
    }
    if let Some(rest) = lower.strip_prefix('+') {
        if let Some(num_str) = rest.strip_suffix('d') {
            if let Ok(n) = num_str.parse::<i64>() {
                if n > 0 && n <= 36_500 {
                    let (ny, nm, nd) = civil_from_days(base_days + n);
                    return Ok(format!("{ny:04}-{nm:02}-{nd:02}"));
                }
            }
        } else if let Some(num_str) = rest.strip_suffix('w') {
            if let Ok(n) = num_str.parse::<i64>() {
                if n > 0 && n <= 5_200 {
                    let (ny, nm, nd) = civil_from_days(base_days + n * 7);
                    return Ok(format!("{ny:04}-{nm:02}-{nd:02}"));
                }
            }
        } else if let Some(num_str) = rest.strip_suffix('m') {
            if let Ok(n) = num_str.parse::<i64>() {
                if n > 0 && n <= 1_200 {
                    let (ny, nm, nd) = add_months(base, n);
                    return Ok(format!("{ny:04}-{nm:02}-{nd:02}"));
                }
            }
        }
    }

    if let Some((y, m, d)) = parse_ymd_tuple(trimmed) {
        return Ok(format!("{y:04}-{m:02}-{d:02}"));
    }
    Err(format!(
        "invalid due date '{trimmed}' (expected YYYY-MM-DD, today, tomorrow, +Nd/+Nw/+Nm, mon..sun, eow, or eom)"
    ))
}

/// Validate and normalize a recurrence rule (`daily`, `weekdays`, `weekly`, `monthly`, `every Nd`/`Nw`/`Nm`).
pub fn parse_repeat(raw: &str) -> Result<String, String> {
    let lower = raw
        .trim()
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match lower.as_str() {
        "daily" | "day" | "1d" | "every day" | "every 1d" => return Ok("daily".into()),
        "weekdays" | "weekday" | "workdays" | "every weekday" => return Ok("weekdays".into()),
        "weekly" | "week" | "1w" | "every week" | "every 1w" => return Ok("weekly".into()),
        "biweekly" | "2w" | "every 2w" | "every 2 weeks" => return Ok("every 2w".into()),
        "monthly" | "month" | "1m" | "every month" | "every 1m" => return Ok("monthly".into()),
        _ => {}
    }
    let body = lower.strip_prefix("every ").unwrap_or(&lower);
    let compact = body.replace(' ', "");
    if let Some(num) = compact
        .strip_suffix("days")
        .or_else(|| compact.strip_suffix("day"))
        .or_else(|| compact.strip_suffix('d'))
    {
        if let Ok(n) = num.parse::<u32>() {
            if n == 1 {
                return Ok("daily".into());
            } else if n > 1 && n <= 3650 {
                return Ok(format!("every {n}d"));
            }
        }
    }
    if let Some(num) = compact
        .strip_suffix("weeks")
        .or_else(|| compact.strip_suffix("week"))
        .or_else(|| compact.strip_suffix('w'))
    {
        if let Ok(n) = num.parse::<u32>() {
            if n == 1 {
                return Ok("weekly".into());
            } else if n > 1 && n <= 520 {
                return Ok(format!("every {n}w"));
            }
        }
    }
    if let Some(num) = compact
        .strip_suffix("months")
        .or_else(|| compact.strip_suffix("month"))
        .or_else(|| compact.strip_suffix('m'))
    {
        if let Ok(n) = num.parse::<u32>() {
            if n == 1 {
                return Ok("monthly".into());
            } else if n > 1 && n <= 120 {
                return Ok(format!("every {n}m"));
            }
        }
    }
    Err(format!(
        "invalid repeat rule '{raw}' (expected: daily, weekdays, weekly, monthly, or every Nd/Nw/Nm)"
    ))
}

pub(crate) fn next_due_for_repeat(
    current_due: Option<&str>,
    repeat: &str,
    today: (i64, u32, u32),
) -> String {
    let today_days = days_from_civil(today.0, today.1, today.2);
    let base = match current_due.and_then(parse_ymd_tuple) {
        Some(due_t) if days_from_civil(due_t.0, due_t.1, due_t.2) > today_days => due_t,
        _ => today,
    };
    let base_days = days_from_civil(base.0, base.1, base.2);

    let norm = parse_repeat(repeat).unwrap_or_else(|_| "daily".into());
    let (ny, nm, nd) = match norm.as_str() {
        "daily" => civil_from_days(base_days + 1),
        "weekdays" => {
            let mut next = base_days + 1;
            while matches!(weekday_from_days(next), 0 | 6) {
                next += 1;
            }
            civil_from_days(next)
        }
        "weekly" => civil_from_days(base_days + 7),
        "monthly" => add_months(base, 1),
        other => {
            let body = other.strip_prefix("every ").unwrap_or(other);
            if let Some(n) = body.strip_suffix('d').and_then(|s| s.parse::<i64>().ok()) {
                civil_from_days(base_days + n)
            } else if let Some(n) = body.strip_suffix('w').and_then(|s| s.parse::<i64>().ok()) {
                civil_from_days(base_days + n * 7)
            } else if let Some(n) = body.strip_suffix('m').and_then(|s| s.parse::<i64>().ok()) {
                add_months(base, n)
            } else {
                civil_from_days(base_days + 1)
            }
        }
    };
    format!("{ny:04}-{nm:02}-{nd:02}")
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
            tasks.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
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
            tasks.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
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
    pub repeat: Option<String>,
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

    /// If this task has a recurrence rule, advance `due` to the next occurrence, reset status to
    /// `Todo`, bump `updated_at`, and return the new due date.
    pub fn complete_if_recurring(&mut self) -> Option<String> {
        let rep = self.repeat.clone()?;
        let next_due = next_due_for_repeat(self.due.as_deref(), &rep, local_today_ymd_tuple());
        self.due = Some(next_due.clone());
        self.status = Status::Todo;
        self.touch();
        Some(next_due)
    }

    /// Count Markdown checklist items (`- [ ]` / `- [x]`) in `description`.
    pub fn checklist_progress(&self) -> Option<(usize, usize)> {
        let mut done = 0usize;
        let mut total = 0usize;
        for line in self.description.lines() {
            if let Some(checked) = parse_checklist_line(line) {
                total += 1;
                if checked {
                    done += 1;
                }
            }
        }
        (total > 0).then_some((done, total))
    }

    /// Toggle the `check_idx`-th Markdown checklist item (`[ ]` <-> `[x]`) in `description`.
    /// Returns `Some(is_now_checked)` on success.
    pub fn toggle_checklist_item(&mut self, check_idx: usize) -> Option<bool> {
        let mut lines: Vec<String> = self.description.lines().map(str::to_string).collect();
        let mut seen = 0usize;
        for line in &mut lines {
            if let Some(checked) = parse_checklist_line(line) {
                if seen == check_idx {
                    let trimmed = line.trim_start();
                    let indent_len = line.len() - trimmed.len();
                    // Find `[ ]` / `[x]` / `[X]` right after the bullet (`- `, `* `, `+ `)
                    let after_bullet_offset = indent_len + 2;
                    let rest = &line[after_bullet_offset..];
                    let ws_len = rest.len() - rest.trim_start().len();
                    let box_start = after_bullet_offset + ws_len;
                    let replacement = if checked { "[ ]" } else { "[x]" };
                    line.replace_range(box_start..box_start + 3, replacement);
                    self.description = lines.join("\n");
                    self.touch();
                    return Some(!checked);
                }
                seen += 1;
            }
        }
        None
    }
}

pub(crate) fn parse_checklist_line(line: &str) -> Option<bool> {
    let trimmed = line.trim_start();
    let rest = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))?;
    let after = rest.trim_start();
    if after.starts_with("[ ]") {
        Some(false)
    } else if after.starts_with("[x]") || after.starts_with("[X]") {
        Some(true)
    } else {
        None
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickAddParsed {
    pub title: String,
    pub priority: Option<Priority>,
    pub tags: Vec<String>,
    pub due: Option<String>,
    pub repeat: Option<String>,
}

/// Parse quick-add shorthand: `Ship release #ops #release !high @tomorrow ~weekly`.
pub fn parse_quick_add(raw: &str) -> Result<QuickAddParsed, String> {
    let mut title_parts: Vec<&str> = Vec::new();
    let mut priority: Option<Priority> = None;
    let mut tags: Vec<String> = Vec::new();
    let mut due: Option<String> = None;
    let mut repeat: Option<String> = None;

    for token in raw.split_whitespace() {
        if let Some(tag_raw) = token.strip_prefix('#') {
            if !tag_raw.is_empty() {
                for t in parse_tags(tag_raw) {
                    if !tags.contains(&t) {
                        tags.push(t);
                    }
                }
                continue;
            }
        } else if let Some(prio_raw) = token.strip_prefix('!') {
            if let Ok(p) = Priority::parse(prio_raw) {
                priority = Some(p);
                continue;
            }
        } else if let Some(due_raw) = token.strip_prefix('@') {
            if !due_raw.is_empty() {
                due = Some(parse_due_date(due_raw)?);
                continue;
            }
        } else if let Some(rep_raw) = token.strip_prefix('~') {
            if !rep_raw.is_empty() {
                repeat = Some(parse_repeat(rep_raw)?);
                continue;
            }
        }
        title_parts.push(token);
    }

    let title = title_parts.join(" ");
    if title.is_empty() {
        return Err("title cannot be empty".into());
    }
    Ok(QuickAddParsed {
        title,
        priority,
        tags,
        due,
        repeat,
    })
}

/// Evaluate a smart search query against a task.
/// Supports operators (`p:high`, `s:todo`, `due:overdue|today|upcoming|any|none`,
/// `repeat:any|none|weekly`, `#tag`, `-done`, `-#tag`, `-word`) alongside plain substring terms.
pub fn matches_search_query(task: &Task, eff_tags: &[String], query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return true;
    }
    let title_lower = task.title.to_lowercase();
    let desc_lower = task.description.to_lowercase();
    let mut today_cache: Option<String> = None;

    for raw_tok in trimmed.split_whitespace() {
        let (negate, tok_raw) = match raw_tok.strip_prefix('-') {
            Some(rest) if !rest.is_empty() => (true, rest),
            _ => (false, raw_tok),
        };
        let tok = tok_raw.to_lowercase();

        let matched = if let Some(val) = tok
            .strip_prefix("p:")
            .or_else(|| tok.strip_prefix("priority:"))
        {
            Priority::parse(val)
                .map(|p| task.priority == p)
                .unwrap_or(false)
        } else if let Some(val) = tok
            .strip_prefix("s:")
            .or_else(|| tok.strip_prefix("status:"))
        {
            Status::parse(val)
                .map(|s| task.status == s)
                .unwrap_or(false)
        } else if let Some(val) = tok.strip_prefix("due:") {
            match val {
                "overdue" | "late" => {
                    let today = today_cache.get_or_insert_with(today_ymd);
                    task.status != Status::Done && task.due_state(today) == Some(DueState::Overdue)
                }
                "today" => {
                    let today = today_cache.get_or_insert_with(today_ymd);
                    task.status != Status::Done && task.due_state(today) == Some(DueState::Today)
                }
                "upcoming" => {
                    let today = today_cache.get_or_insert_with(today_ymd);
                    task.due_state(today) == Some(DueState::Upcoming)
                }
                "any" | "set" => task.due.is_some(),
                "none" => task.due.is_none(),
                other => task.due.as_deref().is_some_and(|d| d.contains(other)),
            }
        } else if let Some(val) = tok
            .strip_prefix("repeat:")
            .or_else(|| tok.strip_prefix("rep:"))
        {
            match val {
                "any" | "set" => task.repeat.is_some(),
                "none" => task.repeat.is_none(),
                other => task.repeat.as_deref().is_some_and(|r| r.contains(other)),
            }
        } else if let Some(tag_q) = tok.strip_prefix('#') {
            !tag_q.is_empty() && eff_tags.iter().any(|t| t.contains(tag_q))
        } else if negate && Status::parse(&tok).is_ok() {
            Status::parse(&tok)
                .map(|s| task.status == s)
                .unwrap_or(false)
        } else {
            title_lower.contains(&tok)
                || desc_lower.contains(&tok)
                || eff_tags.iter().any(|t| t.contains(&tok))
        };

        let ok = if negate { !matched } else { matched };
        if !ok {
            return false;
        }
    }

    true
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
            repeat: None,
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

pub fn archive_path_for(storage_path: &Path) -> PathBuf {
    let file_name = storage_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("todos.jsonl");
    if let Some(stem) = file_name.strip_suffix(".jsonl") {
        if stem.starts_with('.') {
            return storage_path.with_file_name(format!("{stem}.archive.jsonl"));
        }
    }
    storage_path.with_file_name("archive.jsonl")
}

pub fn append_tasks_to_archive(archive_path: &Path, tasks: &[Task]) -> io::Result<()> {
    if tasks.is_empty() {
        return Ok(());
    }
    let _lock = FileLock::acquire(archive_path)?;
    if let Some(parent) = archive_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(archive_path)?;
    for task in tasks {
        let line = serde_json::to_string(task).map_err(io::Error::other)?;
        writeln!(file, "{line}")?;
    }
    Ok(())
}

pub const LOCAL_BOARD_FILENAME: &str = ".roke.jsonl";

/// Walk upward from `start_dir` looking for a project-local `.roke.jsonl` board.
pub fn find_local_board(start_dir: &Path) -> Option<PathBuf> {
    for dir in start_dir.ancestors() {
        let candidate = dir.join(LOCAL_BOARD_FILENAME);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
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
        // Round-trip days_from_civil <-> civil_from_days including leap year
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(
            civil_from_days(days_from_civil(2026, 11, 26)),
            (2026, 11, 26)
        );

        // Natural due dates anchored at Thursday 2026-11-26
        let base = (2026, 11, 26);
        assert_eq!(parse_due_date_from("+3d", base).unwrap(), "2026-11-29");
        assert_eq!(parse_due_date_from("+2w", base).unwrap(), "2026-12-10");
        assert_eq!(
            parse_due_date_from("+1m", (2026, 1, 31)).unwrap(),
            "2026-02-28"
        );
        assert_eq!(parse_due_date_from("fri", base).unwrap(), "2026-11-27");
        assert_eq!(parse_due_date_from("thu", base).unwrap(), "2026-12-03");
        assert_eq!(parse_due_date_from("eow", base).unwrap(), "2026-11-27");
        assert_eq!(parse_due_date_from("eom", base).unwrap(), "2026-11-30");

        // Recurrence rules & next-due computation
        assert_eq!(parse_repeat("daily").unwrap(), "daily");
        assert_eq!(parse_repeat("every 3 days").unwrap(), "every 3d");
        assert_eq!(parse_repeat("biweekly").unwrap(), "every 2w");
        assert!(parse_repeat("hourly").is_err());

        assert_eq!(
            next_due_for_repeat(Some("2026-11-26"), "weekly", base),
            "2026-12-03"
        );
        assert_eq!(
            next_due_for_repeat(Some("2026-11-27"), "weekdays", (2026, 11, 27)),
            "2026-11-30"
        );
        assert_eq!(
            next_due_for_repeat(Some("2026-01-01"), "daily", base),
            "2026-11-27"
        );

        let mut store = TaskStore::new_empty();
        let id = store.add(
            "Tax".into(),
            String::new(),
            Status::Todo,
            Priority::High,
            Vec::new(),
        );
        store.get_mut(id).unwrap().due = Some("2026-01-01".into());
        assert_eq!(
            store.get(id).unwrap().due_state("2026-06-01"),
            Some(DueState::Overdue)
        );
        assert_eq!(
            store.get(id).unwrap().due_state("2026-01-01"),
            Some(DueState::Today)
        );
        assert_eq!(
            store.get(id).unwrap().due_state("2025-12-31"),
            Some(DueState::Upcoming)
        );
    }

    #[test]
    fn find_local_board_walks_parent_directories() {
        let root = std::env::temp_dir().join(format!("roke_local_find_{}", std::process::id()));
        let nested = root.join("packages").join("sub");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(find_local_board(&nested), None);

        let board = root.join(LOCAL_BOARD_FILENAME);
        fs::write(&board, "").unwrap();
        assert_eq!(find_local_board(&nested), Some(board));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn smart_search_query_supports_operators_and_negation() {
        let mut store = TaskStore::new_empty();
        let id1 = store.add(
            "Fix API latency".into(),
            "Check Redis".into(),
            Status::Todo,
            Priority::High,
            vec!["backend".into(), "ops".into()],
        );
        let id2 = store.add(
            "Ship docs".into(),
            String::new(),
            Status::Done,
            Priority::Low,
            vec!["docs".into()],
        );
        store.get_mut(id1).unwrap().due = Some("2020-01-01".into());
        store.get_mut(id1).unwrap().repeat = Some("weekly".into());

        let t1 = store.get(id1).unwrap();
        let t2 = store.get(id2).unwrap();

        assert!(matches_search_query(
            t1,
            &t1.tags,
            "p:high due:overdue #backend"
        ));
        assert!(!matches_search_query(t2, &t2.tags, "p:high"));
        assert!(matches_search_query(t1, &t1.tags, "repeat:weekly -done"));
        assert!(!matches_search_query(t2, &t2.tags, "-done"));
        assert!(!matches_search_query(t1, &t1.tags, "-#ops"));
        assert!(matches_search_query(t1, &t1.tags, "api -mysql"));
        assert!(!matches_search_query(t1, &t1.tags, "api -redis"));
    }
}
