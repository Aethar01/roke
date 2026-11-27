use crate::config::{normalize_device_tag, PeerConfig};
use crate::model::{now_millis, parse_due_date, Priority, Status, Task, TaskStore};
use crate::sync::http::{build_agent, read_response_string};
use crate::sync::oauth::{format_rfc3339_ms, parse_rfc3339_ms, resolve_access_token, url_encode};
use crate::sync::PushError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_GTASKS_BASE: &str = "https://tasks.googleapis.com/tasks/v1";

#[derive(Debug, Clone)]
pub(crate) struct RemoteGoogleTask {
    pub remote_id: String,
    pub task: Task,
}

#[derive(Debug, Clone)]
pub(crate) struct GoogleTasksSnapshot {
    pub api_base: String,
    pub source_list: Option<String>,
    pub list_id: String,
    pub store: TaskStore,
    pub remote_items: HashMap<String, RemoteGoogleTask>, // roke uid -> RemoteGoogleTask
    pub state_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GoogleTasksPeerState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_list: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolved_list_id: Option<String>,
    #[serde(default)]
    seen: HashMap<String, SeenGoogleTask>, // roke uid -> metadata
}

/// Local-only `roke` fields that Google Tasks API v1 does not store.
/// Preserved across syncs so remote edits in Google Tasks never wipe local metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LocalExtras {
    #[serde(default = "default_priority")]
    priority: Priority,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    due_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repeat: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    reminders: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    percent_complete: Option<u8>,
    #[serde(default)]
    in_progress: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    order: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<u64>,
}

fn default_priority() -> Priority {
    Priority::Medium
}

impl LocalExtras {
    fn from_task(task: &Task) -> Self {
        Self {
            priority: task.priority,
            tags: task.tags.clone(),
            due_time: task.due_time.clone(),
            start: task.start.clone(),
            start_time: task.start_time.clone(),
            repeat: task.repeat.clone(),
            reminders: task.reminders.clone(),
            parent_uid: task.parent_uid.clone(),
            location: task.location.clone(),
            url: task.url.clone(),
            percent_complete: task.percent_complete,
            in_progress: task.status == Status::InProgress,
            order: task.order,
            origin: task.origin.clone(),
            created_at: task.created_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SeenGoogleTask {
    remote_id: String,
    synced_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remote_updated: Option<String>,
    #[serde(default)]
    local_updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    extras: Option<LocalExtras>,
}

impl GoogleTasksPeerState {
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
        fs::write(&tmp, json)
            .map_err(|e| format!("failed writing google-tasks sync state: {e}"))?;
        fs::rename(&tmp, path).map_err(|e| format!("failed saving google-tasks sync state: {e}"))
    }
}

fn state_file_path(state_dir: Option<&Path>, peer: &PeerConfig) -> Option<PathBuf> {
    let dir = state_dir?;
    let slug = normalize_device_tag(&peer.name);
    let safe = if slug.is_empty() {
        "google-tasks"
    } else {
        &slug
    };
    Some(dir.join(format!("google-tasks-{safe}.json")))
}

fn api_base(peer: &PeerConfig) -> String {
    peer.url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_GTASKS_BASE.to_string())
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
                format!("Google Tasks GET failed (HTTP {code}): {body}")
            }
            ureq::Error::Transport(t) => format!("Google Tasks GET failed: {t}"),
        })?;
    let body = read_response_string(resp)?;
    serde_json::from_str(&body).map_err(|e| format!("invalid Google Tasks JSON: {e}"))
}

fn discover_list_id(
    agent: &ureq::Agent,
    base: &str,
    wanted_list: Option<&str>,
    token: &str,
) -> Result<String, String> {
    if wanted_list.is_none() {
        return Ok("@default".to_string());
    }
    let wanted = wanted_list.unwrap();
    let mut lists: Vec<(String, String)> = Vec::new();
    let mut page_token: Option<String> = None;

    loop {
        let mut url = format!("{base}/users/@me/lists?maxResults=100");
        if let Some(ref pt) = page_token {
            url.push_str(&format!("&pageToken={}", url_encode(pt)));
        }
        let val = get_json(agent, &url, token)?;
        if let Some(arr) = val.get("items").and_then(Value::as_array) {
            for item in arr {
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    let title = item
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    lists.push((id.to_string(), title));
                }
            }
        }
        page_token = val
            .get("nextPageToken")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if page_token.is_none() {
            break;
        }
    }

    let wl = wanted.to_ascii_lowercase();
    if let Some((id, _)) = lists
        .iter()
        .find(|(id, title)| title.to_ascii_lowercase() == wl || id == wanted)
    {
        return Ok(id.clone());
    }

    let avail = lists
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "Google Tasks list '{wanted}' not found (available: {avail})"
    ))
}

fn google_json_to_task(
    item: &Value,
    fallback_origin: &str,
    seen: Option<(&str, &SeenGoogleTask)>,
) -> Option<(String, Task)> {
    if item
        .get("deleted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let remote_id = item.get("id").and_then(Value::as_str)?.to_string();
    let uid = seen
        .map(|(u, _)| u.to_string())
        .unwrap_or_else(|| format!("gtasks-{remote_id}"));
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("(untitled)")
        .trim()
        .to_string();
    let description = item
        .get("notes")
        .and_then(Value::as_str)
        .unwrap_or("")
        .replace("\r\n", "\n")
        .trim_end()
        .to_string();

    let extras = seen.and_then(|(_, s)| s.extras.as_ref());

    let remote_done = item
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|s| s == "completed");
    let status = if remote_done {
        Status::Done
    } else if extras.is_some_and(|e| e.in_progress) {
        Status::InProgress
    } else {
        Status::Todo
    };

    let due = item
        .get("due")
        .and_then(Value::as_str)
        .and_then(|d| d.get(0..10))
        .and_then(|s| parse_due_date(s).ok());
    let due_time = if due.is_some() {
        extras.and_then(|e| e.due_time.clone())
    } else {
        None
    };

    let completed_at = if status == Status::Done {
        item.get("completed")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_ms)
    } else {
        None
    };

    let remote_updated = item
        .get("updated")
        .and_then(Value::as_str)
        .map(str::to_string);
    let updated_at = match (seen, remote_updated.as_deref()) {
        (Some((_, s)), Some(ru))
            if s.remote_updated.as_deref() == Some(ru) && s.local_updated_at > 0 =>
        {
            s.local_updated_at
        }
        (_, Some(ru)) => parse_rfc3339_ms(ru).unwrap_or_else(now_millis),
        _ => now_millis(),
    };

    let origin = extras.and_then(|e| e.origin.clone()).or_else(|| {
        let slug = normalize_device_tag(fallback_origin);
        (!slug.is_empty()).then_some(slug)
    });

    Some((
        remote_id,
        Task {
            id: 0,
            uid,
            title,
            description,
            status,
            priority: extras.map(|e| e.priority).unwrap_or(Priority::Medium),
            tags: extras.map(|e| e.tags.clone()).unwrap_or_default(),
            due,
            due_time,
            start: extras.and_then(|e| e.start.clone()),
            start_time: extras.and_then(|e| e.start_time.clone()),
            repeat: extras.and_then(|e| e.repeat.clone()),
            reminders: extras.map(|e| e.reminders.clone()).unwrap_or_default(),
            parent_uid: extras.and_then(|e| e.parent_uid.clone()),
            location: extras.and_then(|e| e.location.clone()),
            url: extras.and_then(|e| e.url.clone()),
            percent_complete: extras.and_then(|e| e.percent_complete),
            created_at: extras.and_then(|e| e.created_at),
            completed_at,
            updated_at,
            origin,
            order: extras.and_then(|e| e.order),
        },
    ))
}

fn task_to_google_payload(task: &Task) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("title".into(), json!(task.title));
    obj.insert("notes".into(), json!(task.description));
    if task.status == Status::Done {
        obj.insert("status".into(), json!("completed"));
        let done_ts = task.completed_at.unwrap_or(task.updated_at);
        obj.insert("completed".into(), json!(format_rfc3339_ms(done_ts)));
    } else {
        obj.insert("status".into(), json!("needsAction"));
        obj.insert("completed".into(), Value::Null);
    }
    if let Some(ref due) = task.due {
        obj.insert("due".into(), json!(format!("{due}T00:00:00.000Z")));
    } else {
        obj.insert("due".into(), Value::Null);
    }
    Value::Object(obj)
}

/// Compare only the fields Google Tasks API v1 actually supports so local-only edits
/// (tags, priority, etc.) don't trigger redundant PATCH requests.
fn tasks_google_equal(a: &Task, b: &Task) -> bool {
    a.title == b.title
        && a.description == b.description
        && (a.status == Status::Done) == (b.status == Status::Done)
        && a.due == b.due
}

pub(crate) fn pull(
    peer: &PeerConfig,
    state_dir: Option<&Path>,
) -> Result<GoogleTasksSnapshot, String> {
    let base = api_base(peer);
    let source_list = normalized_list_name(peer);
    let token = resolve_access_token(peer, state_dir)?;
    let agent = build_agent();

    let state_path = state_file_path(state_dir, peer);
    let prev_state = state_path
        .as_deref()
        .map(GoogleTasksPeerState::load)
        .unwrap_or_default();

    let list_id = if prev_state.source_list == source_list {
        match prev_state.resolved_list_id.clone() {
            Some(id) => id,
            None => discover_list_id(&agent, &base, source_list.as_deref(), &token)?,
        }
    } else {
        discover_list_id(&agent, &base, source_list.as_deref(), &token)?
    };

    let by_remote_id: HashMap<&str, (&str, &SeenGoogleTask)> = prev_state
        .seen
        .iter()
        .map(|(uid, s)| (s.remote_id.as_str(), (uid.as_str(), s)))
        .collect();

    let mut store = TaskStore::new_empty();
    let mut remote_items: HashMap<String, RemoteGoogleTask> = HashMap::new();
    let mut page_token: Option<String> = None;
    let list_seg = url_encode(&list_id);

    loop {
        let mut url = format!(
            "{base}/lists/{list_seg}/tasks?showCompleted=true&showHidden=true&maxResults=100"
        );
        if let Some(ref pt) = page_token {
            url.push_str(&format!("&pageToken={}", url_encode(pt)));
        }
        let val = get_json(&agent, &url, &token)?;
        if let Some(arr) = val.get("items").and_then(Value::as_array) {
            for item in arr {
                let seen_match = item
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|rid| by_remote_id.get(rid).copied());
                if let Some((remote_id, task)) = google_json_to_task(item, &peer.name, seen_match) {
                    let uid = task.uid.clone();
                    store.tasks.push(task.clone());
                    remote_items.insert(uid, RemoteGoogleTask { remote_id, task });
                }
            }
        }
        page_token = val
            .get("nextPageToken")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if page_token.is_none() {
            break;
        }
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

    Ok(GoogleTasksSnapshot {
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
    snapshot: &GoogleTasksSnapshot,
    state_dir: Option<&Path>,
) -> Result<(), PushError> {
    let token = resolve_access_token(peer, state_dir).map_err(PushError::Other)?;
    let agent = build_agent();
    let sync_ts = now_millis();

    let mut next_seen: HashMap<String, SeenGoogleTask> = HashMap::new();
    let merged_uids: HashSet<&str> = merged.tasks.iter().map(|t| t.uid.as_str()).collect();
    let list_seg = url_encode(&snapshot.list_id);

    for task in &merged.tasks {
        let seen_ts = sync_ts.max(task.updated_at);
        let extras = Some(LocalExtras::from_task(task));
        if let Some(remote) = snapshot.remote_items.get(&task.uid) {
            if tasks_google_equal(task, &remote.task) {
                next_seen.insert(
                    task.uid.clone(),
                    SeenGoogleTask {
                        remote_id: remote.remote_id.clone(),
                        synced_at_ms: seen_ts,
                        remote_updated: None,
                        local_updated_at: task.updated_at,
                        extras,
                    },
                );
                continue;
            }
            let url = format!(
                "{}/lists/{list_seg}/tasks/{}",
                snapshot.api_base,
                url_encode(&remote.remote_id)
            );
            let payload = task_to_google_payload(task);
            let resp = agent
                .request("PATCH", &url)
                .set("Authorization", &format!("Bearer {token}"))
                .set("Content-Type", "application/json")
                .send_string(&payload.to_string())
                .map_err(|e| match e {
                    ureq::Error::Status(code, r) => {
                        let body = read_response_string(r).unwrap_or_default();
                        PushError::Other(format!("Google Tasks PATCH failed (HTTP {code}): {body}"))
                    }
                    ureq::Error::Transport(t) => {
                        PushError::Other(format!("Google Tasks PATCH failed: {t}"))
                    }
                })?;
            let resp_body = read_response_string(resp).unwrap_or_default();
            let remote_updated = serde_json::from_str::<Value>(&resp_body)
                .ok()
                .and_then(|v| v.get("updated").and_then(Value::as_str).map(str::to_string));
            next_seen.insert(
                task.uid.clone(),
                SeenGoogleTask {
                    remote_id: remote.remote_id.clone(),
                    synced_at_ms: seen_ts,
                    remote_updated,
                    local_updated_at: task.updated_at,
                    extras,
                },
            );
        } else {
            let url = format!("{}/lists/{list_seg}/tasks", snapshot.api_base);
            let payload = task_to_google_payload(task);
            let resp = agent
                .post(&url)
                .set("Authorization", &format!("Bearer {token}"))
                .set("Content-Type", "application/json")
                .send_string(&payload.to_string())
                .map_err(|e| match e {
                    ureq::Error::Status(code, r) => {
                        let body = read_response_string(r).unwrap_or_default();
                        PushError::Other(format!("Google Tasks POST failed (HTTP {code}): {body}"))
                    }
                    ureq::Error::Transport(t) => {
                        PushError::Other(format!("Google Tasks POST failed: {t}"))
                    }
                })?;
            let resp_body = read_response_string(resp).map_err(PushError::Other)?;
            let created: Value = serde_json::from_str(&resp_body)
                .map_err(|e| PushError::Other(format!("invalid Google Tasks POST JSON: {e}")))?;
            let remote_id = created
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| PushError::Other("Google Tasks POST missing 'id'".into()))?
                .to_string();
            let remote_updated = created
                .get("updated")
                .and_then(Value::as_str)
                .map(str::to_string);
            next_seen.insert(
                task.uid.clone(),
                SeenGoogleTask {
                    remote_id,
                    synced_at_ms: seen_ts,
                    remote_updated,
                    local_updated_at: task.updated_at,
                    extras,
                },
            );
        }
    }

    for (uid, remote) in &snapshot.remote_items {
        if merged_uids.contains(uid.as_str()) {
            continue;
        }
        let url = format!(
            "{}/lists/{list_seg}/tasks/{}",
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
                    "Google Tasks DELETE failed (HTTP {code}): {body}"
                )));
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(PushError::Other(format!("Google Tasks DELETE failed: {t}")));
            }
        }
    }

    if let Some(ref sp) = snapshot.state_path {
        let state = GoogleTasksPeerState {
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

    struct MockGoogleTasks {
        tasks: HashMap<String, Value>,
        seq: u64,
    }

    fn spawn_mock_gtasks(state: Arc<Mutex<MockGoogleTasks>>) -> String {
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
                    ("GET", "/lists/%40default/tasks") => {
                        let vals: Vec<Value> = s.tasks.values().cloned().collect();
                        let resp_json = json!({ "items": vals }).to_string();
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            resp_json.len(),
                            resp_json
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    ("POST", "/lists/%40default/tasks") => {
                        s.seq += 1;
                        let id = format!("gt-{}", s.seq);
                        let mut item: Value = serde_json::from_str(&body_str).unwrap();
                        item["id"] = json!(id);
                        item["updated"] = json!("2026-11-27T10:00:00.000Z");
                        s.tasks.insert(id, item.clone());
                        let resp_json = item.to_string();
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            resp_json.len(),
                            resp_json
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    ("PATCH", p) if p.starts_with("/lists/%40default/tasks/") => {
                        let id = p.trim_start_matches("/lists/%40default/tasks/");
                        let patch: Value = serde_json::from_str(&body_str).unwrap();
                        let entry = s
                            .tasks
                            .entry(id.to_string())
                            .or_insert_with(|| json!({ "id": id }));
                        if let (Some(dst), Some(src)) = (entry.as_object_mut(), patch.as_object()) {
                            for (k, v) in src {
                                dst.insert(k.clone(), v.clone());
                            }
                            dst.insert("updated".into(), json!("2026-11-27T11:00:00.000Z"));
                        }
                        let resp_json = entry.to_string();
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            resp_json.len(),
                            resp_json
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    ("DELETE", p) if p.starts_with("/lists/%40default/tasks/") => {
                        let id = p.trim_start_matches("/lists/%40default/tasks/");
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
    fn google_tasks_sync_preserves_local_metadata_when_edited_remotely() {
        let state_dir =
            std::env::temp_dir().join(format!("roke_gtasks_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&state_dir);

        let server = Arc::new(Mutex::new(MockGoogleTasks {
            tasks: HashMap::new(),
            seq: 0,
        }));
        let base_url = spawn_mock_gtasks(Arc::clone(&server));

        let mut local = TaskStore::new_empty();
        let id = local.add_with_origin(
            "Call dentist".into(),
            "Ask about Friday".into(),
            Status::InProgress,
            Priority::High,
            vec!["health".into()],
            "laptop",
        );
        {
            let t = local.get_mut(id).unwrap();
            t.due = Some("2026-12-01".into());
            t.due_time = Some("14:30".into());
            t.repeat = Some("weekly".into());
            t.updated_at = 1_764_140_000_000;
        }

        let cfg = SyncConfig {
            device_name: Some("laptop".into()),
            peers: vec![PeerConfig {
                url: Some(base_url),
                token_cmd: Some("printf test-token".into()),
                ..PeerConfig::minimal("gtasks", PeerKind::GoogleTasks)
            }],
            ..SyncConfig::default()
        };

        // Sync 1: Creates task on Google Tasks and records local extras
        let out1 = sync_all_peers(local, cfg.clone(), Some(state_dir.clone()));
        assert!(out1.errors.is_empty(), "errors: {:?}", out1.errors);
        assert_eq!(server.lock().unwrap().tasks.len(), 1);

        // Simulate editing the title on Android Google Tasks (far-future updated timestamp)
        {
            let mut guard = server.lock().unwrap();
            let entry = guard.tasks.get_mut("gt-1").unwrap();
            entry["title"] = json!("Call dentist (confirm insurance)");
            entry["updated"] = json!("2099-01-01T00:00:00.000Z");
        }

        // Sync 2: Pulls remote title update while preserving local-only roke metadata!
        let out2 = sync_all_peers(out1.merged_store, cfg.clone(), Some(state_dir.clone()));
        assert!(out2.errors.is_empty(), "errors: {:?}", out2.errors);
        assert_eq!(out2.merged_store.tasks.len(), 1);
        let t = &out2.merged_store.tasks[0];
        assert_eq!(t.title, "Call dentist (confirm insurance)");
        assert_eq!(t.status, Status::InProgress);
        assert_eq!(t.priority, Priority::High);
        assert_eq!(t.tags, vec!["health"]);
        assert_eq!(t.due_time.as_deref(), Some("14:30"));
        assert_eq!(t.repeat.as_deref(), Some("weekly"));

        // Delete on Google Tasks -> propagates cleanly to local
        server.lock().unwrap().tasks.remove("gt-1");
        let out3 = sync_all_peers(out2.merged_store, cfg, Some(state_dir.clone()));
        assert!(out3.errors.is_empty(), "errors: {:?}", out3.errors);
        assert!(out3.merged_store.tasks.is_empty());

        let _ = fs::remove_dir_all(&state_dir);
    }
}
