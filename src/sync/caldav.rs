use crate::config::{normalize_device_tag, PeerConfig};
use crate::model::{now_millis, Task, TaskStore};
use crate::sync::http::{
    apply_auth, basic_auth_header, build_agent, build_no_redirect_agent, normalize_etag,
    parse_discovery_multistatus, parse_multistatus, read_response_string, resolve_href, url_origin,
    DavDiscoveryEntry,
};
use crate::sync::ical::{patch_vtodo, task_to_vtodo, vtodo_to_task};
use crate::sync::PushError;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

const CALDAV_REPORT_BODY: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VTODO"/>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#;

const CALDAV_PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:getetag/>
  </D:prop>
</D:propfind>"#;

const CALDAV_DISCOVERY_PROPFIND: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:resourcetype/>
    <D:displayname/>
    <D:current-user-principal/>
    <C:calendar-home-set/>
    <C:supported-calendar-component-set/>
  </D:prop>
</D:propfind>"#;

#[derive(Debug, Clone)]
pub(crate) struct RemoteVtodo {
    pub href: String,
    pub etag: Option<String>,
    pub raw_ics: String,
    pub task: Task,
}

#[derive(Debug, Clone)]
pub(crate) struct CalDavSnapshot {
    pub source_url: String,
    pub source_list: Option<String>,
    pub collection_url: String,
    pub store: TaskStore,
    pub remote_items: HashMap<String, RemoteVtodo>,
    pub state_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CalDavPeerState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_list: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolved_collection_url: Option<String>,
    #[serde(default)]
    seen: HashMap<String, SeenVtodo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SeenVtodo {
    href: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    synced_at_ms: u64,
}

impl CalDavPeerState {
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
        fs::write(&tmp, json).map_err(|e| format!("failed writing sync state: {e}"))?;
        fs::rename(&tmp, path).map_err(|e| format!("failed saving sync state: {e}"))
    }
}

fn state_file_path(state_dir: Option<&Path>, peer: &PeerConfig) -> Option<PathBuf> {
    let dir = state_dir?;
    let slug = normalize_device_tag(&peer.name);
    let safe = if slug.is_empty() { "caldav" } else { &slug };
    Some(dir.join(format!("caldav-{safe}.json")))
}

fn ensure_trailing_slash(url: &str) -> String {
    if url.ends_with('/') {
        url.to_string()
    } else {
        format!("{url}/")
    }
}

fn raw_peer_url(peer: &PeerConfig) -> Result<String, String> {
    peer.url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "missing 'url' for caldav peer".to_string())
}

fn normalized_list_name(peer: &PeerConfig) -> Option<String> {
    peer.list
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn last_path_segment(url_or_href: &str) -> String {
    let trimmed = url_or_href.trim_end_matches('/');
    let seg = trimmed.rsplit('/').next().unwrap_or(trimmed);
    percent_decode(seg)
}

enum DavHttpError {
    Status(u16, String),
    Transport(String),
}

impl From<ureq::Error> for DavHttpError {
    fn from(err: ureq::Error) -> Self {
        match err {
            ureq::Error::Status(code, resp) => {
                DavHttpError::Status(code, resp.status_text().to_string())
            }
            ureq::Error::Transport(t) => DavHttpError::Transport(t.to_string()),
        }
    }
}

/// Send a `PROPFIND` while manually following HTTP 301/302/303/307/308 redirects so the
/// `PROPFIND` method, `Depth` header, and XML body are preserved (standard for `/.well-known/caldav`).
fn propfind_discovery(
    url: &str,
    depth: &str,
    auth: Option<&str>,
) -> Result<(String, Vec<DavDiscoveryEntry>), DavHttpError> {
    let agent = build_no_redirect_agent();
    let mut current_url = url.to_string();

    for _ in 0..5 {
        let req = apply_auth(
            agent
                .request("PROPFIND", &current_url)
                .set("Depth", depth)
                .set("Content-Type", "application/xml; charset=utf-8"),
            auth,
        );
        let resp = req.send_string(CALDAV_DISCOVERY_PROPFIND)?;
        let status = resp.status();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            if let Some(loc) = resp.header("Location") {
                current_url = resolve_href(&current_url, loc);
                continue;
            }
        }
        let xml = read_response_string(resp).unwrap_or_default();
        return Ok((current_url, parse_discovery_multistatus(&xml)));
    }

    Ok((current_url, Vec::new()))
}

fn select_task_collection(
    home_url: &str,
    entries: &[DavDiscoveryEntry],
    wanted_list: Option<&str>,
) -> Result<String, String> {
    let home_norm = home_url.trim_end_matches('/');
    let mut candidates: Vec<(String, String, bool)> = Vec::new(); // (url, label, explicit_vtodo)

    for entry in entries {
        if !entry.is_calendar {
            continue;
        }
        if entry.supports_vtodo == Some(false) {
            continue;
        }
        let full_url = ensure_trailing_slash(&resolve_href(home_url, &entry.href));
        if full_url.trim_end_matches('/') == home_norm {
            continue;
        }
        let slug = last_path_segment(&full_url);
        let label = entry
            .displayname
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| slug.clone());
        let explicit_vtodo = entry.supports_vtodo == Some(true);
        candidates.push((full_url, label, explicit_vtodo));
    }

    if candidates.is_empty() {
        return Err(format!(
            "no CalDAV task (VTODO) collections found under {home_url}"
        ));
    }

    if let Some(wanted) = wanted_list {
        let wanted_lower = wanted.to_ascii_lowercase();
        if let Some((url, _, _)) = candidates.iter().find(|(u, label, _)| {
            label.to_ascii_lowercase() == wanted_lower
                || last_path_segment(u).to_ascii_lowercase() == wanted_lower
        }) {
            return Ok(url.clone());
        }
        let available = candidates
            .iter()
            .map(|(_, label, _)| label.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "CalDAV list '{wanted}' not found (available: {available})"
        ));
    }

    // Prefer a collection explicitly advertising VTODO, otherwise pick the first calendar.
    if let Some((url, _, _)) = candidates.iter().find(|(_, _, explicit)| *explicit) {
        return Ok(url.clone());
    }
    Ok(candidates[0].0.clone())
}

/// Auto-discover a CalDAV VTODO task-list collection URL from any root URL, `/.well-known/caldav`,
/// principal URL, calendar-home-set URL, or direct collection URL (RFC 6764 / RFC 4791).
pub(crate) fn discover_collection_url(
    raw_url: &str,
    wanted_list: Option<&str>,
    auth: Option<&str>,
) -> Result<String, String> {
    let probe = match propfind_discovery(raw_url, "0", auth) {
        Ok((final_url, entries)) if !entries.is_empty() => Some((final_url, entries)),
        Ok(_) => None,
        Err(DavHttpError::Status(code @ (401 | 403), text)) => {
            return Err(format!("CalDAV authentication failed: HTTP {code} {text}"));
        }
        Err(DavHttpError::Status(405 | 501, _)) if wanted_list.is_none() => {
            // Minimal server doesn't support discovery PROPFIND; treat URL as direct collection
            return Ok(ensure_trailing_slash(raw_url));
        }
        Err(DavHttpError::Transport(t)) => {
            return Err(format!("CalDAV request failed: {t}"));
        }
        Err(_) => None,
    };

    // If initial URL didn't yield discovery props, try `/.well-known/caldav` on the same origin.
    let (base_url, entries) = match probe {
        Some((u, e))
            if e.iter().any(|x| {
                x.is_calendar || x.calendar_home_set.is_some() || x.current_user_principal.is_some()
            }) =>
        {
            (u, e)
        }
        other => {
            let well_known_res = url_origin(raw_url).and_then(|origin| {
                let wk = format!("{origin}/.well-known/caldav");
                propfind_discovery(&wk, "0", auth).ok()
            });
            match well_known_res {
                Some((u, e)) if !e.is_empty() => (u, e),
                _ => match other {
                    Some(pair) => pair,
                    None => return Ok(ensure_trailing_slash(raw_url)),
                },
            }
        }
    };

    // Case 1: User already pointed directly at a calendar/task collection.
    if let Some(first) = entries.first() {
        if first.is_calendar && wanted_list.is_none() {
            return Ok(ensure_trailing_slash(&base_url));
        }
    }

    // Case 2: Resolve calendar-home-set (directly or via current-user-principal).
    let mut home_set: Option<String> = entries
        .iter()
        .find_map(|e| e.calendar_home_set.as_deref())
        .map(|h| ensure_trailing_slash(&resolve_href(&base_url, h)));

    if home_set.is_none() {
        if let Some(principal_href) = entries
            .iter()
            .find_map(|e| e.current_user_principal.as_deref())
        {
            let principal_url = resolve_href(&base_url, principal_href);
            if let Ok((p_final, p_entries)) = propfind_discovery(&principal_url, "0", auth) {
                home_set = p_entries
                    .iter()
                    .find_map(|e| e.calendar_home_set.as_deref())
                    .map(|h| ensure_trailing_slash(&resolve_href(&p_final, h)));
            }
        }
    }

    // Fallback: the configured URL itself may already be the calendar-home-set (e.g. Radicale `/user/`).
    let home_url = home_set.unwrap_or_else(|| ensure_trailing_slash(&base_url));

    match propfind_discovery(&home_url, "1", auth) {
        Ok((h_final, children)) => select_task_collection(&h_final, &children, wanted_list),
        Err(DavHttpError::Status(code, text)) => Err(format!(
            "CalDAV discovery PROPFIND {home_url} failed: HTTP {code} {text}"
        )),
        Err(DavHttpError::Transport(t)) => Err(format!("CalDAV discovery failed: {t}")),
    }
}

fn new_item_url(collection_url: &str, uid: &str) -> String {
    let safe_uid: String = uid
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let base = if safe_uid.is_empty() {
        "task".to_string()
    } else {
        safe_uid
    };
    format!("{collection_url}{base}.ics")
}

/// Compare two tasks on the fields persisted to CalDAV (ignoring sub-second timestamp jitter).
fn tasks_caldav_equal(a: &Task, b: &Task) -> bool {
    a.title == b.title
        && a.description == b.description
        && a.status == b.status
        && a.priority == b.priority
        && a.tags == b.tags
        && a.due == b.due
        && a.due_time == b.due_time
        && a.repeat == b.repeat
        && a.order == b.order
        && a.origin == b.origin
        && (a.completed_at.map(|ms| ms / 1000) == b.completed_at.map(|ms| ms / 1000))
}

fn fetch_all_items_via_propfind(
    agent: &ureq::Agent,
    collection: &str,
    auth: Option<&str>,
) -> Result<Vec<(String, Option<String>, String)>, String> {
    let req = apply_auth(
        agent
            .request("PROPFIND", collection)
            .set("Depth", "1")
            .set("Content-Type", "application/xml; charset=utf-8"),
        auth,
    );
    let resp = req
        .send_string(CALDAV_PROPFIND_BODY)
        .map_err(|e| format!("CalDAV PROPFIND failed: {e}"))?;
    let xml = read_response_string(resp)?;
    let responses = parse_multistatus(&xml);

    let mut items = Vec::new();
    for entry in responses {
        let full_url = resolve_href(collection, &entry.href);
        if full_url.trim_end_matches('/') == collection.trim_end_matches('/')
            || !full_url.to_ascii_lowercase().ends_with(".ics")
        {
            continue;
        }
        let get_req = apply_auth(agent.get(&full_url), auth);
        match get_req.call() {
            Ok(get_resp) => {
                let etag = get_resp.header("ETag").map(normalize_etag).or(entry.etag);
                let body = read_response_string(get_resp)?;
                items.push((full_url, etag, body));
            }
            Err(ureq::Error::Status(404, _)) => continue,
            Err(e) => return Err(format!("CalDAV GET {full_url} failed: {e}")),
        }
    }
    Ok(items)
}

fn fetch_collection_items(
    agent: &ureq::Agent,
    collection: &str,
    auth: Option<&str>,
) -> Result<Vec<(String, Option<String>, String)>, DavHttpError> {
    let report_req = apply_auth(
        agent
            .request("REPORT", collection)
            .set("Depth", "1")
            .set("Content-Type", "application/xml; charset=utf-8"),
        auth,
    );

    let resp = report_req.send_string(CALDAV_REPORT_BODY)?;
    let xml = read_response_string(resp).unwrap_or_default();
    let mut parsed = Vec::new();
    for entry in parse_multistatus(&xml) {
        let full_url = resolve_href(collection, &entry.href);
        if let Some(cal_data) = entry.calendar_data {
            if !cal_data.trim().is_empty() {
                parsed.push((full_url, entry.etag, cal_data));
                continue;
            }
        }
        if full_url.trim_end_matches('/') != collection.trim_end_matches('/')
            && full_url.to_ascii_lowercase().ends_with(".ics")
        {
            let get_req = apply_auth(agent.get(&full_url), auth);
            if let Ok(get_resp) = get_req.call() {
                let etag = get_resp.header("ETag").map(normalize_etag).or(entry.etag);
                if let Ok(body) = read_response_string(get_resp) {
                    parsed.push((full_url, etag, body));
                }
            }
        }
    }
    Ok(parsed)
}

pub(crate) fn pull(peer: &PeerConfig, state_dir: Option<&Path>) -> Result<CalDavSnapshot, String> {
    let source_url = raw_peer_url(peer)?;
    let source_list = normalized_list_name(peer);
    let auth = basic_auth_header(peer)?;
    let agent = build_agent();

    let state_path = state_file_path(state_dir, peer);
    let prev_state = state_path
        .as_deref()
        .map(CalDavPeerState::load)
        .unwrap_or_default();

    let cached_collection = if prev_state.source_url.as_deref() == Some(source_url.as_str())
        && prev_state.source_list == source_list
    {
        prev_state.resolved_collection_url.clone()
    } else {
        None
    };

    let (mut collection, used_cache) = match cached_collection {
        Some(url) => (url, true),
        None => (
            discover_collection_url(&source_url, source_list.as_deref(), auth.as_deref())?,
            false,
        ),
    };

    let raw_items = match fetch_collection_items(&agent, &collection, auth.as_deref()) {
        Ok(items) => items,
        Err(DavHttpError::Status(404 | 410, _)) if used_cache => {
            // Cached collection URL went stale; re-discover and retry once
            collection =
                discover_collection_url(&source_url, source_list.as_deref(), auth.as_deref())?;
            match fetch_collection_items(&agent, &collection, auth.as_deref()) {
                Ok(items) => items,
                Err(DavHttpError::Status(400 | 405 | 501, _)) => {
                    fetch_all_items_via_propfind(&agent, &collection, auth.as_deref())?
                }
                Err(DavHttpError::Status(code, text)) => {
                    return Err(format!("CalDAV REPORT failed: HTTP {code} {text}"));
                }
                Err(DavHttpError::Transport(t)) => {
                    return Err(format!("CalDAV request failed: {t}"));
                }
            }
        }
        Err(DavHttpError::Status(400 | 405 | 501, _)) => {
            fetch_all_items_via_propfind(&agent, &collection, auth.as_deref())?
        }
        Err(DavHttpError::Status(code, text)) => {
            return Err(format!("CalDAV REPORT failed: HTTP {code} {text}"));
        }
        Err(DavHttpError::Transport(t)) => {
            return Err(format!("CalDAV request failed: {t}"));
        }
    };

    let mut store = TaskStore::new_empty();
    let mut remote_items: HashMap<String, RemoteVtodo> = HashMap::new();

    for (href, etag, raw_ics) in raw_items {
        if let Ok(task) = vtodo_to_task(&raw_ics, &peer.name) {
            let uid = task.uid.clone();
            store.tasks.push(task.clone());
            remote_items.insert(
                uid,
                RemoteVtodo {
                    href,
                    etag,
                    raw_ics,
                    task,
                },
            );
        }
    }

    // Synthesize tombstones for UIDs that existed at last sync on this same collection
    // but disappeared from the server (e.g., deleted in Tasks.org).
    let same_collection = prev_state.resolved_collection_url.as_deref()
        == Some(collection.as_str())
        || prev_state.resolved_collection_url.is_none();
    if same_collection {
        for (uid, seen) in prev_state.seen {
            if !remote_items.contains_key(&uid) {
                store.tombstones.insert(uid, seen.synced_at_ms);
            }
        }
    }

    store.ensure_sync_metadata(&peer.name);

    Ok(CalDavSnapshot {
        source_url,
        source_list,
        collection_url: collection,
        store,
        remote_items,
        state_path,
    })
}

pub(crate) fn push(
    peer: &PeerConfig,
    merged: &TaskStore,
    snapshot: &CalDavSnapshot,
) -> Result<(), PushError> {
    let auth = basic_auth_header(peer).map_err(PushError::Other)?;
    let agent = build_agent();
    let sync_ts = now_millis();

    let mut next_seen: HashMap<String, SeenVtodo> = HashMap::new();
    let merged_uids: HashSet<&str> = merged.tasks.iter().map(|t| t.uid.as_str()).collect();

    // 1. PUT newly created or updated tasks
    for task in &merged.tasks {
        let seen_ts = sync_ts.max(task.updated_at);
        if let Some(remote) = snapshot.remote_items.get(&task.uid) {
            if tasks_caldav_equal(task, &remote.task) {
                next_seen.insert(
                    task.uid.clone(),
                    SeenVtodo {
                        href: remote.href.clone(),
                        etag: remote.etag.clone(),
                        synced_at_ms: seen_ts,
                    },
                );
                continue;
            }
            let body = patch_vtodo(&remote.raw_ics, task);
            let mut req = apply_auth(
                agent
                    .put(&remote.href)
                    .set("Content-Type", "text/calendar; charset=utf-8"),
                auth.as_deref(),
            );
            if let Some(ref etag) = remote.etag {
                if !etag.is_empty() {
                    req = req.set("If-Match", etag);
                }
            }
            match req.send_string(&body) {
                Ok(resp) => {
                    let new_etag = resp
                        .header("ETag")
                        .map(normalize_etag)
                        .or_else(|| remote.etag.clone());
                    next_seen.insert(
                        task.uid.clone(),
                        SeenVtodo {
                            href: remote.href.clone(),
                            etag: new_etag,
                            synced_at_ms: seen_ts,
                        },
                    );
                }
                Err(ureq::Error::Status(412, _)) => return Err(PushError::Conflict),
                Err(ureq::Error::Status(code, resp)) => {
                    return Err(PushError::Other(format!(
                        "CalDAV PUT {} failed: HTTP {code} {}",
                        remote.href,
                        resp.status_text()
                    )));
                }
                Err(ureq::Error::Transport(t)) => {
                    return Err(PushError::Other(format!("CalDAV PUT failed: {t}")));
                }
            }
        } else {
            let item_url = new_item_url(&snapshot.collection_url, &task.uid);
            let body = task_to_vtodo(task);
            let req = apply_auth(
                agent
                    .put(&item_url)
                    .set("Content-Type", "text/calendar; charset=utf-8")
                    .set("If-None-Match", "*"),
                auth.as_deref(),
            );
            match req.send_string(&body) {
                Ok(resp) => {
                    let new_etag = resp.header("ETag").map(normalize_etag);
                    next_seen.insert(
                        task.uid.clone(),
                        SeenVtodo {
                            href: item_url,
                            etag: new_etag,
                            synced_at_ms: seen_ts,
                        },
                    );
                }
                Err(ureq::Error::Status(412, _)) => return Err(PushError::Conflict),
                Err(ureq::Error::Status(code, resp)) => {
                    return Err(PushError::Other(format!(
                        "CalDAV PUT {item_url} failed: HTTP {code} {}",
                        resp.status_text()
                    )));
                }
                Err(ureq::Error::Transport(t)) => {
                    return Err(PushError::Other(format!("CalDAV PUT failed: {t}")));
                }
            }
        }
    }

    // 2. DELETE tasks removed/archived in roke (tombstoned locally or absent from merged store)
    for (uid, remote) in &snapshot.remote_items {
        if merged_uids.contains(uid.as_str()) {
            continue;
        }
        let mut req = apply_auth(agent.delete(&remote.href), auth.as_deref());
        if let Some(ref etag) = remote.etag {
            if !etag.is_empty() {
                req = req.set("If-Match", etag);
            }
        }
        match req.call() {
            Ok(_) | Err(ureq::Error::Status(404, _)) => {}
            Err(ureq::Error::Status(412, _)) => return Err(PushError::Conflict),
            Err(ureq::Error::Status(code, resp)) => {
                return Err(PushError::Other(format!(
                    "CalDAV DELETE {} failed: HTTP {code} {}",
                    remote.href,
                    resp.status_text()
                )));
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(PushError::Other(format!("CalDAV DELETE failed: {t}")));
            }
        }
    }

    if let Some(ref sp) = snapshot.state_path {
        let state = CalDavPeerState {
            source_url: Some(snapshot.source_url.clone()),
            source_list: snapshot.source_list.clone(),
            resolved_collection_url: Some(snapshot.collection_url.clone()),
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
    use crate::model::{Priority, Status};
    use crate::sync::sync_all_peers;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    struct MockCalDav {
        items: HashMap<String, (String, String)>, // path -> (etag, ics)
        seq: u64,
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    fn spawn_mock_caldav(state: Arc<Mutex<MockCalDav>>) -> String {
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
                let mut depth = String::new();
                let mut if_match: Option<String> = None;
                let mut if_none_match: Option<String> = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        let kl = k.trim().to_ascii_lowercase();
                        let val = v.trim().to_string();
                        if kl == "content-length" {
                            content_len = val.parse().unwrap_or(0);
                        } else if kl == "depth" {
                            depth = val;
                        } else if kl == "if-match" {
                            if_match = Some(val);
                        } else if kl == "if-none-match" {
                            if_none_match = Some(val);
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
                match (method, path) {
                    ("PROPFIND", "/.well-known/caldav") => {
                        let resp = "HTTP/1.1 301 Moved Permanently\r\nLocation: /dav/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    ("PROPFIND", "/") => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                    ("PROPFIND", "/dav/") => {
                        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/dav/</D:href>
    <D:propstat>
      <D:prop>
        <D:current-user-principal><D:href>/dav/principals/elliott/</D:href></D:current-user-principal>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
                        let resp = format!(
                            "HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            xml.len(),
                            xml
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    ("PROPFIND", "/dav/principals/elliott/") => {
                        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/dav/principals/elliott/</D:href>
    <D:propstat>
      <D:prop>
        <C:calendar-home-set><D:href>/dav/calendars/elliott/</D:href></C:calendar-home-set>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
                        let resp = format!(
                            "HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            xml.len(),
                            xml
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    ("PROPFIND", "/dav/calendars/elliott/") if depth == "1" => {
                        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/dav/calendars/elliott/</D:href>
    <D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/calendars/elliott/events/</D:href>
    <D:propstat><D:prop>
      <D:displayname>Events</D:displayname>
      <D:resourcetype><D:collection/><C:calendar/></D:resourcetype>
      <C:supported-calendar-component-set><C:comp name="VEVENT"/></C:supported-calendar-component-set>
    </D:prop></D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/calendars/elliott/default/</D:href>
    <D:propstat><D:prop>
      <D:displayname>Default list</D:displayname>
      <D:resourcetype><D:collection/><C:calendar/></D:resourcetype>
      <C:supported-calendar-component-set><C:comp name="VTODO"/></C:supported-calendar-component-set>
    </D:prop></D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/calendars/elliott/work/</D:href>
    <D:propstat><D:prop>
      <D:displayname>Work</D:displayname>
      <D:resourcetype><D:collection/><C:calendar/></D:resourcetype>
      <C:supported-calendar-component-set><C:comp name="VTODO"/></C:supported-calendar-component-set>
    </D:prop></D:propstat>
  </D:response>
</D:multistatus>"#;
                        let resp = format!(
                            "HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            xml.len(),
                            xml
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    ("PROPFIND", p)
                        if p.starts_with("/user/tasks/")
                            || p.starts_with("/dav/calendars/elliott/") =>
                    {
                        let xml = format!(
                            r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>{p}</D:href>
    <D:propstat><D:prop>
      <D:resourcetype><D:collection/><C:calendar/></D:resourcetype>
      <C:supported-calendar-component-set><C:comp name="VTODO"/></C:supported-calendar-component-set>
    </D:prop></D:propstat>
  </D:response>
</D:multistatus>"#
                        );
                        let resp = format!(
                            "HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            xml.len(),
                            xml
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    ("REPORT", collection_path) => {
                        let mut xml = String::from(
                            r#"<?xml version="1.0" encoding="utf-8"?><D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">"#,
                        );
                        for (item_path, (etag, ics)) in &s.items {
                            if item_path.starts_with(collection_path) {
                                xml.push_str(&format!(
                                    "<D:response><D:href>{item_path}</D:href><D:propstat><D:prop><D:getetag>{}</D:getetag><C:calendar-data>{}</C:calendar-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
                                    xml_escape(etag),
                                    xml_escape(ics)
                                ));
                            }
                        }
                        xml.push_str("</D:multistatus>");
                        let resp = format!(
                            "HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            xml.len(),
                            xml
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    ("PUT", _) => {
                        if if_none_match.as_deref() == Some("*") && s.items.contains_key(path) {
                            let _ = stream.write_all(
                                b"HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            );
                            continue;
                        }
                        if let Some(ref exp) = if_match {
                            if s.items.get(path).map(|(e, _)| e.as_str()) != Some(exp.as_str()) {
                                let _ = stream.write_all(
                                    b"HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                );
                                continue;
                            }
                        }
                        s.seq += 1;
                        let etag = format!("\"e{}\"", s.seq);
                        s.items.insert(path.to_string(), (etag.clone(), body_str));
                        let resp = format!(
                            "HTTP/1.1 201 Created\r\nETag: {etag}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    ("DELETE", _) => {
                        if let Some(ref exp) = if_match {
                            if s.items.get(path).map(|(e, _)| e.as_str()) != Some(exp.as_str()) {
                                let _ = stream.write_all(
                                    b"HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                );
                                continue;
                            }
                        }
                        s.items.remove(path);
                        let _ = stream.write_all(
                            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                    _ => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                }
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn caldav_autodiscovery_resolves_well_known_principal_homeset_and_named_list() {
        let server_state = Arc::new(Mutex::new(MockCalDav {
            items: HashMap::new(),
            seq: 0,
        }));
        let origin = spawn_mock_caldav(Arc::clone(&server_state));

        // Bare domain root -> /.well-known/caldav -> /dav/ -> principal -> home-set -> skips VEVENT -> picks Default list
        let default_col = discover_collection_url(&origin, None, None).unwrap();
        assert_eq!(
            default_col,
            format!("{origin}/dav/calendars/elliott/default/")
        );

        // Named list selection (case-insensitive)
        let work_col = discover_collection_url(&origin, Some("work"), None).unwrap();
        assert_eq!(work_col, format!("{origin}/dav/calendars/elliott/work/"));

        // Unknown list reports available lists cleanly
        let err = discover_collection_url(&origin, Some("Groceries"), None).unwrap_err();
        assert!(err.contains("Default list"));
        assert!(err.contains("Work"));
        assert!(!err.contains("Events"));
    }

    #[test]
    fn caldav_two_way_sync_preserves_valarm_and_propagates_deletions_both_ways() {
        let state_dir =
            std::env::temp_dir().join(format!("roke_caldav_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&state_dir);

        let phone_ics = concat!(
            "BEGIN:VCALENDAR\r\n",
            "VERSION:2.0\r\n",
            "BEGIN:VTODO\r\n",
            "UID:phone-1\r\n",
            "SUMMARY:Buy coffee\r\n",
            "STATUS:NEEDS-ACTION\r\n",
            "PRIORITY:1\r\n",
            "CATEGORIES:errands\r\n",
            "LAST-MODIFIED:20261126T090000Z\r\n",
            "BEGIN:VALARM\r\n",
            "ACTION:DISPLAY\r\n",
            "TRIGGER:-PT10M\r\n",
            "END:VALARM\r\n",
            "END:VTODO\r\n",
            "END:VCALENDAR\r\n",
        );

        let mut items = HashMap::new();
        items.insert(
            "/user/tasks/phone-1.ics".to_string(),
            ("\"e1\"".to_string(), phone_ics.to_string()),
        );
        let server_state = Arc::new(Mutex::new(MockCalDav { items, seq: 1 }));
        let origin = spawn_mock_caldav(Arc::clone(&server_state));
        let url = format!("{origin}/user/tasks/");

        let mut local = TaskStore::new_empty();
        let laptop_id = local.add_with_origin(
            "Ship roke".into(),
            "- [ ] Add CalDAV".into(),
            Status::InProgress,
            Priority::High,
            vec!["dev".into()],
            "laptop",
        );
        let laptop_uid = local.get(laptop_id).unwrap().uid.clone();

        let cfg = SyncConfig {
            device_name: Some("laptop".into()),
            peers: vec![PeerConfig {
                url: Some(url),
                username: Some("elliott".into()),
                password: Some("pass".into()),
                ..PeerConfig::minimal("tasks-org", PeerKind::Caldav)
            }],
            ..SyncConfig::default()
        };

        // Sync 1: Pulls phone-1 and pushes laptop task
        let out1 = sync_all_peers(local, cfg.clone(), Some(state_dir.clone()));
        assert!(out1.errors.is_empty(), "errors: {:?}", out1.errors);
        assert_eq!(out1.synced_peers, 1);
        assert_eq!(out1.merged_store.tasks.len(), 2);
        assert_eq!(server_state.lock().unwrap().items.len(), 2);

        // Edit phone-1 locally and delete laptop task locally (simulates delete/archive)
        let mut local2 = out1.merged_store;
        let p_id = local2.find_id("phone-1").unwrap();
        local2.get_mut(p_id).unwrap().title = "Buy espresso beans".into();
        local2.get_mut(p_id).unwrap().touch();
        let l_id = local2.find_id(&laptop_uid).unwrap();
        local2.remove(l_id);

        // Sync 2: Patches phone-1 (keeping VALARM!) and DELETEs laptop task on server
        let out2 = sync_all_peers(local2, cfg.clone(), Some(state_dir.clone()));
        assert!(out2.errors.is_empty(), "errors: {:?}", out2.errors);
        assert_eq!(out2.merged_store.tasks.len(), 1);
        {
            let guard = server_state.lock().unwrap();
            assert_eq!(guard.items.len(), 1);
            let (_, ics) = guard.items.get("/user/tasks/phone-1.ics").unwrap();
            assert!(ics.contains("SUMMARY:Buy espresso beans"));
            assert!(ics.contains("BEGIN:VALARM"));
            assert!(ics.contains("TRIGGER:-PT10M"));
        }

        // Now simulate deleting phone-1 on the phone (Tasks.org removes it from server)
        server_state
            .lock()
            .unwrap()
            .items
            .remove("/user/tasks/phone-1.ics");

        // Sync 3: Local roke detects the deletion via state file and removes phone-1 instead of resurrecting it
        let out3 = sync_all_peers(out2.merged_store, cfg, Some(state_dir.clone()));
        assert!(out3.errors.is_empty(), "errors: {:?}", out3.errors);
        assert!(
            out3.merged_store.tasks.is_empty(),
            "task deleted on phone should not resurrect"
        );
        assert!(server_state.lock().unwrap().items.is_empty());

        let _ = fs::remove_dir_all(&state_dir);
    }
}
