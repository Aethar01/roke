use crate::config::{normalize_device_tag, PeerConfig};
use crate::model::{
    now_millis, parse_due_date, parse_due_time, parse_tags, Priority, Status, Task, TaskStore,
};
use crate::sync::http::{build_agent, read_response_string};
use crate::sync::oauth::{format_rfc3339_ms, parse_rfc3339_ms, resolve_access_token, url_encode};
use crate::sync::PushError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

#[derive(Debug, Clone)]
pub(crate) struct RemoteMsTask {
    pub remote_id: String,
    pub task: Task,
}

#[derive(Debug, Clone)]
pub(crate) struct MsTodoSnapshot {
    pub api_base: String,
    pub source_list: Option<String>,
    pub list_id: String,
    pub store: TaskStore,
    pub remote_items: HashMap<String, RemoteMsTask>, // roke uid -> RemoteMsTask
    pub state_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct MsTodoPeerState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_list: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolved_list_id: Option<String>,
    #[serde(default)]
    seen: HashMap<String, SeenMsTask>, // roke uid -> metadata
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SeenMsTask {
    remote_id: String,
    synced_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remote_modified: Option<String>,
    #[serde(default)]
    local_updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    order: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
}

impl MsTodoPeerState {
    fn load(path: &Path) -> Self {
        let Ok(raw) = fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("failed creating sync state dir: {e}"))?;
            }
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        fs::write(&tmp, json).map_err(|e| format!("failed writing ms-todo sync state: {e}"))?;
        fs::rename(&tmp, path).map_err(|e| format!("failed saving ms-todo sync state: {e}"))
    }
}

fn state_file_path(state_dir: Option<&Path>, peer: &PeerConfig) -> Option<PathBuf> {
    let dir = state_dir?;
    let slug = normalize_device_tag(&peer.name);
    let safe = if slug.is_empty() { "ms-todo" } else { &slug };
    Some(dir.join(format!("ms-todo-{safe}.json")))
}

fn api_base(peer: &PeerConfig) -> String {
    peer.url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_GRAPH_BASE.to_string())
}

fn normalized_list_name(peer: &PeerConfig) -> Option<String> {
    peer.list
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn get_json(agent: &ureq::Agent, url: &str, token: &str) -> Result<Value, String> {
    let resp = agent
        .get(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/json")
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, r) => {
                let body = read_response_string(r).unwrap_or_default();
                format!("Microsoft To Do GET failed (HTTP {code}): {body}")
            }
            ureq::Error::Transport(t) => format!("Microsoft To Do GET failed: {t}"),
        })?;
    let body = read_response_string(resp)?;
    serde_json::from_str(&body).map_err(|e| format!("invalid Microsoft Graph JSON: {e}"))
}

fn discover_list_id(
    agent: &ureq::Agent,
    base: &str,
    wanted_list: Option<&str>,
    token: &str,
) -> Result<String, String> {
    let mut lists: Vec<(String, String, Option<String>)> = Vec::new(); // (id, displayName, wellknown)
    let mut next_url = Some(format!("{base}/me/todo/lists"));

    while let Some(url) = next_url {
        let val = get_json(agent, &url, token)?;
        if let Some(arr) = val.get("value").and_then(Value::as_array) {
            for item in arr {
                let Some(id) = item.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let name = item
                    .get("displayName")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let wk = item
                    .get("wellknownListName")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                lists.push((id.to_string(), name, wk));
            }
        }
        next_url = val
            .get("@odata.nextLink")
            .and_then(Value::as_str)
            .map(str::to_string);
    }

    if lists.is_empty() {
        return Err("no Microsoft To Do lists found on this account".into());
    }

    if let Some(wanted) = wanted_list {
        let wl = wanted.to_ascii_lowercase();
        if let Some((id, _, _)) = lists
            .iter()
            .find(|(id, name, _)| name.to_ascii_lowercase() == wl || id == wanted)
        {
            return Ok(id.clone());
        }
        let avail = lists
            .iter()
            .map(|(_, name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "Microsoft To Do list '{wanted}' not found (available: {avail})"
        ));
    }

    if let Some((id, _, _)) = lists
        .iter()
        .find(|(_, _, wk)| wk.as_deref() == Some("defaultList"))
    {
        return Ok(id.clone());
    }
    Ok(lists[0].0.clone())
}

fn ms_status_to_roke(status: &str) -> Status {
    match status {
        "completed" => Status::Done,
        "inProgress" | "waitingOnOthers" | "deferred" => Status::InProgress,
        _ => Status::Todo,
    }
}

fn roke_status_to_ms(status: Status) -> &'static str {
    match status {
        Status::Todo => "notStarted",
        Status::InProgress => "inProgress",
        Status::Done => "completed",
    }
}

fn ms_importance_to_roke(imp: &str) -> Priority {
    match imp {
        "high" => Priority::High,
        "low" => Priority::Low,
        _ => Priority::Medium,
    }
}

fn roke_priority_to_ms(p: Priority) -> &'static str {
    match p {
        Priority::High => "high",
        Priority::Medium => "normal",
        Priority::Low => "low",
    }
}

fn ms_recurrence_to_roke(rec: &Value) -> Option<String> {
    let pattern = rec.get("pattern")?;
    let ptype = pattern.get("type").and_then(Value::as_str)?;
    let interval = pattern.get("interval").and_then(Value::as_u64).unwrap_or(1) as u32;
    match (ptype, interval) {
        ("daily", 1) => Some("daily".into()),
        ("daily", n) if n > 1 => Some(format!("every {n}d")),
        ("weekly", 1) => {
            if let Some(days) = pattern.get("daysOfWeek").and_then(Value::as_array) {
                let mut names: Vec<String> = days
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.to_ascii_lowercase())
                    .collect();
                names.sort();
                if names == ["friday", "monday", "thursday", "tuesday", "wednesday"] {
                    return Some("weekdays".into());
                }
            }
            Some("weekly".into())
        }
        ("weekly", n) if n > 1 => Some(format!("every {n}w")),
        ("absoluteMonthly" | "relativeMonthly", 1) => Some("monthly".into()),
        ("absoluteMonthly" | "relativeMonthly", n) if n > 1 => Some(format!("every {n}m")),
        _ => None,
    }
}

fn roke_repeat_to_ms(repeat: &str, due: Option<&str>) -> Option<Value> {
    let start_date = due.unwrap_or("2026-01-01");
    let pattern = match repeat.trim() {
        "daily" => json!({ "type": "daily", "interval": 1 }),
        "weekdays" => json!({
            "type": "weekly",
            "interval": 1,
            "daysOfWeek": ["monday", "tuesday", "wednesday", "thursday", "friday"]
        }),
        "weekly" => json!({ "type": "weekly", "interval": 1 }),
        "monthly" => json!({ "type": "absoluteMonthly", "interval": 1, "dayOfMonth": 1 }),
        other => {
            let body = other.strip_prefix("every ")?;
            if let Some(n) = body.strip_suffix('d').and_then(|s| s.parse::<u32>().ok()) {
                json!({ "type": "daily", "interval": n })
            } else if let Some(n) = body.strip_suffix('w').and_then(|s| s.parse::<u32>().ok()) {
                json!({ "type": "weekly", "interval": n })
            } else {
                let n = body.strip_suffix('m').and_then(|s| s.parse::<u32>().ok())?;
                json!({ "type": "absoluteMonthly", "interval": n, "dayOfMonth": 1 })
            }
        }
    };
    Some(json!({
        "pattern": pattern,
        "range": {
            "type": "noEnd",
            "startDate": start_date
        }
    }))
}

fn extract_ymd(dt: &str) -> Option<String> {
    let prefix = dt.get(0..10)?;
    parse_due_date(prefix).ok()
}

fn extract_hm(dt: &str) -> Option<String> {
    let hm = dt.get(11..16)?;
    parse_due_time(hm).ok()
}

fn ms_json_to_task(
    item: &Value,
    fallback_origin: &str,
    seen: Option<(&str, &SeenMsTask)>,
) -> Option<(String, Task)> {
    let remote_id = item.get("id").and_then(Value::as_str)?.to_string();
    let uid = seen
        .map(|(u, _)| u.to_string())
        .unwrap_or_else(|| format!("ms-{remote_id}"));
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("(untitled)")
        .trim()
        .to_string();
    let description = item
        .get("body")
        .and_then(|b| b.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .replace("\r\n", "\n")
        .trim_end()
        .to_string();
    let status = item
        .get("status")
        .and_then(Value::as_str)
        .map(ms_status_to_roke)
        .unwrap_or(Status::Todo);
    let priority = item
        .get("importance")
        .and_then(Value::as_str)
        .map(ms_importance_to_roke)
        .unwrap_or(Priority::Medium);
    let mut tags = Vec::new();
    if let Some(cats) = item.get("categories").and_then(Value::as_array) {
        for c in cats {
            if let Some(raw) = c.as_str() {
                for t in parse_tags(raw) {
                    if !tags.contains(&t) {
                        tags.push(t);
                    }
                }
            }
        }
    }
    let due = item
        .get("dueDateTime")
        .and_then(|d| d.get("dateTime"))
        .and_then(Value::as_str)
        .and_then(extract_ymd);
    let is_reminder_on = item
        .get("isReminderOn")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let due_time = if is_reminder_on {
        item.get("reminderDateTime")
            .and_then(|r| r.get("dateTime"))
            .and_then(Value::as_str)
            .and_then(extract_hm)
    } else {
        None
    };
    let repeat = item.get("recurrence").and_then(ms_recurrence_to_roke);
    let created_at = item
        .get("createdDateTime")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_ms);
    let completed_at = if status == Status::Done {
        item.get("completedDateTime")
            .and_then(|c| c.get("dateTime"))
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_ms)
    } else {
        None
    };

    let remote_modified = item
        .get("lastModifiedDateTime")
        .and_then(Value::as_str)
        .map(str::to_string);
    let updated_at = match (seen, remote_modified.as_deref()) {
        (Some((_, s)), Some(rm))
            if s.remote_modified.as_deref() == Some(rm) && s.local_updated_at > 0 =>
        {
            s.local_updated_at
        }
        (_, Some(rm)) => parse_rfc3339_ms(rm).unwrap_or_else(now_millis),
        _ => created_at.unwrap_or_else(now_millis),
    };

    let origin = seen.and_then(|(_, s)| s.origin.clone()).or_else(|| {
        let slug = normalize_device_tag(fallback_origin);
        (!slug.is_empty()).then_some(slug)
    });
    let order = seen.and_then(|(_, s)| s.order);

    Some((
        remote_id,
        Task {
            id: 0,
            uid,
            title,
            description,
            status,
            priority,
            tags,
            due,
            due_time,
            repeat,
            created_at,
            completed_at,
            updated_at,
            origin,
            order,
        },
    ))
}

fn task_to_ms_payload(task: &Task) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("title".into(), json!(task.title));
    obj.insert(
        "body".into(),
        json!({ "content": task.description, "contentType": "text" }),
    );
    obj.insert("status".into(), json!(roke_status_to_ms(task.status)));
    obj.insert(
        "importance".into(),
        json!(roke_priority_to_ms(task.priority)),
    );
    obj.insert("categories".into(), json!(task.tags));
    if let Some(ref due) = task.due {
        obj.insert(
            "dueDateTime".into(),
            json!({ "dateTime": format!("{due}T00:00:00"), "timeZone": "UTC" }),
        );
        if let Some(ref hm) = task.due_time {
            obj.insert("isReminderOn".into(), json!(true));
            obj.insert(
                "reminderDateTime".into(),
                json!({ "dateTime": format!("{due}T{hm}:00"), "timeZone": "UTC" }),
            );
        } else {
            obj.insert("isReminderOn".into(), json!(false));
        }
    } else {
        obj.insert("dueDateTime".into(), Value::Null);
        obj.insert("isReminderOn".into(), json!(false));
    }
    if task.status == Status::Done {
        let done_ts = task.completed_at.unwrap_or(task.updated_at);
        obj.insert(
            "completedDateTime".into(),
            json!({ "dateTime": format_rfc3339_ms(done_ts), "timeZone": "UTC" }),
        );
    }
    if let Some(ref rep) = task.repeat {
        if let Some(rec) = roke_repeat_to_ms(rep, task.due.as_deref()) {
            obj.insert("recurrence".into(), rec);
        }
    } else {
        obj.insert("recurrence".into(), Value::Null);
    }
    Value::Object(obj)
}

fn tasks_ms_equal(a: &Task, b: &Task) -> bool {
    a.title == b.title
        && a.description == b.description
        && a.status == b.status
        && a.priority == b.priority
        && a.tags == b.tags
        && a.due == b.due
        && a.due_time == b.due_time
        && a.repeat == b.repeat
}

pub(crate) fn pull(peer: &PeerConfig, state_dir: Option<&Path>) -> Result<MsTodoSnapshot, String> {
    let base = api_base(peer);
    let source_list = normalized_list_name(peer);
    let token = resolve_access_token(peer, state_dir)?;
    let agent = build_agent();

    let state_path = state_file_path(state_dir, peer);
    let prev_state = state_path
        .as_deref()
        .map(MsTodoPeerState::load)
        .unwrap_or_default();

    let list_id = if prev_state.source_list == source_list {
        match prev_state.resolved_list_id.clone() {
            Some(id) => id,
            None => discover_list_id(&agent, &base, source_list.as_deref(), &token)?,
        }
    } else {
        discover_list_id(&agent, &base, source_list.as_deref(), &token)?
    };

    let by_remote_id: HashMap<&str, (&str, &SeenMsTask)> = prev_state
        .seen
        .iter()
        .map(|(uid, s)| (s.remote_id.as_str(), (uid.as_str(), s)))
        .collect();

    let mut store = TaskStore::new_empty();
    let mut remote_items: HashMap<String, RemoteMsTask> = HashMap::new();
    let mut next_url = Some(format!(
        "{base}/me/todo/lists/{}/tasks?$top=100",
        url_encode(&list_id)
    ));

    while let Some(url) = next_url {
        let val = get_json(&agent, &url, &token)?;
        if let Some(arr) = val.get("value").and_then(Value::as_array) {
            for item in arr {
                let seen_match = item
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|rid| by_remote_id.get(rid).copied());
                if let Some((remote_id, task)) = ms_json_to_task(item, &peer.name, seen_match) {
                    let uid = task.uid.clone();
                    store.tasks.push(task.clone());
                    remote_items.insert(uid, RemoteMsTask { remote_id, task });
                }
            }
        }
        next_url = val
            .get("@odata.nextLink")
            .and_then(Value::as_str)
            .map(str::to_string);
    }

    let same_list = prev_state.resolved_list_id.as_deref() == Some(list_id.as_str())
        || prev_state.resolved_list_id.is_none();
    if same_list {
        for (uid, seen) in prev_state.seen {
            if !remote_items.contains_key(&uid) {
                store.tombstones.insert(uid, seen.synced_at_ms);
            }
        }
    }

    store.ensure_sync_metadata(&peer.name);

    Ok(MsTodoSnapshot {
        api_base: base,
        source_list,
        list_id,
        store,
        remote_items,
        state_path,
    })
}

pub(crate) fn push(
    peer: &PeerConfig,
    merged: &TaskStore,
    snapshot: &MsTodoSnapshot,
    state_dir: Option<&Path>,
) -> Result<(), PushError> {
    let token = resolve_access_token(peer, state_dir).map_err(PushError::Other)?;
    let agent = build_agent();
    let sync_ts = now_millis();

    let mut next_seen: HashMap<String, SeenMsTask> = HashMap::new();
    let merged_uids: HashSet<&str> = merged.tasks.iter().map(|t| t.uid.as_str()).collect();
    let list_seg = url_encode(&snapshot.list_id);

    for task in &merged.tasks {
        let seen_ts = sync_ts.max(task.updated_at);
        if let Some(remote) = snapshot.remote_items.get(&task.uid) {
            if tasks_ms_equal(task, &remote.task) {
                next_seen.insert(
                    task.uid.clone(),
                    SeenMsTask {
                        remote_id: remote.remote_id.clone(),
                        synced_at_ms: seen_ts,
                        remote_modified: None,
                        local_updated_at: task.updated_at,
                        order: task.order,
                        origin: task.origin.clone(),
                    },
                );
                continue;
            }
            let url = format!(
                "{}/me/todo/lists/{list_seg}/tasks/{}",
                snapshot.api_base,
                url_encode(&remote.remote_id)
            );
            let payload = task_to_ms_payload(task);
            let resp = agent
                .request("PATCH", &url)
                .set("Authorization", &format!("Bearer {token}"))
                .set("Content-Type", "application/json")
                .send_string(&payload.to_string())
                .map_err(|e| match e {
                    ureq::Error::Status(code, r) => {
                        let body = read_response_string(r).unwrap_or_default();
                        PushError::Other(format!(
                            "Microsoft To Do PATCH failed (HTTP {code}): {body}"
                        ))
                    }
                    ureq::Error::Transport(t) => {
                        PushError::Other(format!("Microsoft To Do PATCH failed: {t}"))
                    }
                })?;
            let resp_body = read_response_string(resp).unwrap_or_default();
            let remote_modified = serde_json::from_str::<Value>(&resp_body)
                .ok()
                .and_then(|v| {
                    v.get("lastModifiedDateTime")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            next_seen.insert(
                task.uid.clone(),
                SeenMsTask {
                    remote_id: remote.remote_id.clone(),
                    synced_at_ms: seen_ts,
                    remote_modified,
                    local_updated_at: task.updated_at,
                    order: task.order,
                    origin: task.origin.clone(),
                },
            );
        } else {
            let url = format!("{}/me/todo/lists/{list_seg}/tasks", snapshot.api_base);
            let payload = task_to_ms_payload(task);
            let resp = agent
                .post(&url)
                .set("Authorization", &format!("Bearer {token}"))
                .set("Content-Type", "application/json")
                .send_string(&payload.to_string())
                .map_err(|e| match e {
                    ureq::Error::Status(code, r) => {
                        let body = read_response_string(r).unwrap_or_default();
                        PushError::Other(format!(
                            "Microsoft To Do POST failed (HTTP {code}): {body}"
                        ))
                    }
                    ureq::Error::Transport(t) => {
                        PushError::Other(format!("Microsoft To Do POST failed: {t}"))
                    }
                })?;
            let resp_body = read_response_string(resp).map_err(PushError::Other)?;
            let created: Value = serde_json::from_str(&resp_body)
                .map_err(|e| PushError::Other(format!("invalid Microsoft To Do POST JSON: {e}")))?;
            let remote_id = created
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| PushError::Other("Microsoft To Do POST missing 'id'".into()))?
                .to_string();
            let remote_modified = created
                .get("lastModifiedDateTime")
                .and_then(Value::as_str)
                .map(str::to_string);
            next_seen.insert(
                task.uid.clone(),
                SeenMsTask {
                    remote_id,
                    synced_at_ms: seen_ts,
                    remote_modified,
                    local_updated_at: task.updated_at,
                    order: task.order,
                    origin: task.origin.clone(),
                },
            );
        }
    }

    for (uid, remote) in &snapshot.remote_items {
        if merged_uids.contains(uid.as_str()) {
            continue;
        }
        let url = format!(
            "{}/me/todo/lists/{list_seg}/tasks/{}",
            snapshot.api_base,
            url_encode(&remote.remote_id)
        );
        match agent
            .delete(&url)
            .set("Authorization", &format!("Bearer {token}"))
            .call()
        {
            Ok(_) | Err(ureq::Error::Status(404, _)) => {}
            Err(ureq::Error::Status(code, r)) => {
                let body = read_response_string(r).unwrap_or_default();
                return Err(PushError::Other(format!(
                    "Microsoft To Do DELETE failed (HTTP {code}): {body}"
                )));
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(PushError::Other(format!(
                    "Microsoft To Do DELETE failed: {t}"
                )));
            }
        }
    }

    if let Some(ref sp) = snapshot.state_path {
        let state = MsTodoPeerState {
            source_list: snapshot.source_list.clone(),
            resolved_list_id: Some(snapshot.list_id.clone()),
            seen: next_seen,
        };
        state.save(sp).map_err(PushError::Other)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PeerKind, SyncConfig};
    use crate::sync::sync_all_peers;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    struct MockGraph {
        tasks: HashMap<String, Value>,
        seq: u64,
    }

    fn spawn_mock_graph(state: Arc<Mutex<MockGraph>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut req_line = String::new();
                if reader.read_line(&mut req_line).is_err() || req_line.is_empty() {
                    continue;
                }
                let mut parts = req_line.split_whitespace();
                let method = parts.next().unwrap_or("");
                let path = parts.next().unwrap_or("/");
                let mut content_len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        if k.trim().eq_ignore_ascii_case("content-length") {
                            content_len = v.trim().parse().unwrap_or(0);
                        }
                    }
                }
                let mut body = vec![0u8; content_len];
                if content_len > 0 {
                    let _ = reader.read_exact(&mut body);
                }
                let body_str = String::from_utf8_lossy(&body).to_string();

                let mut s = state.lock().unwrap();
                let mut stream = stream;
                let path_no_q = path.split('?').next().unwrap_or(path);
                match (method, path_no_q) {
                    ("GET", "/me/todo/lists") => {
                        let resp_json = json!({
                            "value": [
                                { "id": "list-default", "displayName": "Tasks", "wellknownListName": "defaultList" },
                                { "id": "list-work", "displayName": "Work" }
                            ]
                        })
                        .to_string();
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            resp_json.len(),
                            resp_json
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    ("GET", "/me/todo/lists/list-default/tasks") => {
                        let vals: Vec<Value> = s.tasks.values().cloned().collect();
                        let resp_json = json!({ "value": vals }).to_string();
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            resp_json.len(),
                            resp_json
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    ("POST", "/me/todo/lists/list-default/tasks") => {
                        s.seq += 1;
                        let id = format!("ms-srv-{}", s.seq);
                        let mut item: Value = serde_json::from_str(&body_str).unwrap();
                        item["id"] = json!(id);
                        item["lastModifiedDateTime"] = json!("2026-11-27T10:00:00Z");
                        s.tasks.insert(id, item.clone());
                        let resp_json = item.to_string();
                        let resp = format!(
                            "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            resp_json.len(),
                            resp_json
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    ("PATCH", p) if p.starts_with("/me/todo/lists/list-default/tasks/") => {
                        let id = p.trim_start_matches("/me/todo/lists/list-default/tasks/");
                        let patch: Value = serde_json::from_str(&body_str).unwrap();
                        let entry = s
                            .tasks
                            .entry(id.to_string())
                            .or_insert_with(|| json!({ "id": id }));
                        if let (Some(dst), Some(src)) = (entry.as_object_mut(), patch.as_object()) {
                            for (k, v) in src {
                                dst.insert(k.clone(), v.clone());
                            }
                            dst.insert(
                                "lastModifiedDateTime".into(),
                                json!("2026-11-27T11:00:00Z"),
                            );
                        }
                        let resp_json = entry.to_string();
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            resp_json.len(),
                            resp_json
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    ("DELETE", p) if p.starts_with("/me/todo/lists/list-default/tasks/") => {
                        let id = p.trim_start_matches("/me/todo/lists/list-default/tasks/");
                        s.tasks.remove(id);
                        let _ = stream.write_all(
                            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                    _ => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                }
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn ms_todo_two_way_sync_handles_all_fields_and_bidirectional_deletes() {
        let state_dir =
            std::env::temp_dir().join(format!("roke_mstodo_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&state_dir);

        let mut tasks = HashMap::new();
        tasks.insert(
            "ms-1".into(),
            json!({
                "id": "ms-1",
                "title": "Review PR on phone",
                "body": { "content": "Check auth flow", "contentType": "text" },
                "status": "inProgress",
                "importance": "high",
                "categories": ["dev", "urgent"],
                "dueDateTime": { "dateTime": "2026-12-01T00:00:00.0000000", "timeZone": "UTC" },
                "isReminderOn": true,
                "reminderDateTime": { "dateTime": "2026-12-01T14:30:00.0000000", "timeZone": "UTC" },
                "lastModifiedDateTime": "2026-11-26T09:00:00Z"
            }),
        );
        let server = Arc::new(Mutex::new(MockGraph { tasks, seq: 1 }));
        let base_url = spawn_mock_graph(Arc::clone(&server));

        let mut local = TaskStore::new_empty();
        let l_id = local.add_with_origin(
            "Buy oat milk".into(),
            String::new(),
            Status::Todo,
            Priority::Low,
            vec!["groceries".into()],
            "laptop",
        );
        let laptop_uid = local.get(l_id).unwrap().uid.clone();

        let cfg = SyncConfig {
            device_name: Some("laptop".into()),
            peers: vec![PeerConfig {
                url: Some(base_url),
                token_cmd: Some("printf test-token".into()),
                ..PeerConfig::minimal("mstodo", PeerKind::MsTodo)
            }],
            ..SyncConfig::default()
        };

        // Sync 1: Pulls ms-1 with full fidelity and creates laptop task on server
        let out1 = sync_all_peers(local, cfg.clone(), Some(state_dir.clone()));
        assert!(out1.errors.is_empty(), "errors: {:?}", out1.errors);
        assert_eq!(out1.merged_store.tasks.len(), 2);
        let remote_t = out1
            .merged_store
            .tasks
            .iter()
            .find(|t| t.title == "Review PR on phone")
            .unwrap();
        assert_eq!(remote_t.status, Status::InProgress);
        assert_eq!(remote_t.priority, Priority::High);
        assert_eq!(remote_t.tags, vec!["dev", "urgent"]);
        assert_eq!(remote_t.due.as_deref(), Some("2026-12-01"));
        assert_eq!(remote_t.due_time.as_deref(), Some("14:30"));
        assert_eq!(server.lock().unwrap().tasks.len(), 2);

        // Delete laptop task locally + delete ms-1 on server
        let mut local2 = out1.merged_store;
        let lid2 = local2.find_id(&laptop_uid).unwrap();
        local2.remove(lid2);
        server.lock().unwrap().tasks.remove("ms-1");

        // Sync 2: Both deletions propagate cleanly in both directions
        let out2 = sync_all_peers(local2, cfg, Some(state_dir.clone()));
        assert!(out2.errors.is_empty(), "errors: {:?}", out2.errors);
        assert!(out2.merged_store.tasks.is_empty());
        assert!(server.lock().unwrap().tasks.is_empty());

        let _ = fs::remove_dir_all(&state_dir);
    }
}
