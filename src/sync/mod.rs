pub mod caldav;
pub mod file;
pub mod google_tasks;
pub mod http;
pub mod ical;
pub mod ms_todo;
pub mod oauth;
pub mod ssh;
pub mod webdav;

use crate::config::{default_state_dir, normalize_device_tag, PeerConfig, PeerKind, SyncConfig};
use crate::model::{default_storage_path, FileLock, Task, TaskStore};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

#[derive(Debug, Clone)]
pub struct SyncOutcome {
    pub merged_store: TaskStore,
    pub synced_peers: usize,
    pub errors: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct WholeFileSnapshot {
    pub store: Option<TaskStore>,
    pub token: String,
}

#[derive(Debug)]
pub(crate) enum RemoteSnapshot {
    WholeFile(WholeFileSnapshot),
    CalDav(caldav::CalDavSnapshot),
    MsTodo(ms_todo::MsTodoSnapshot),
    GoogleTasks(google_tasks::GoogleTasksSnapshot),
}

impl RemoteSnapshot {
    fn remote_store(&self) -> Option<&TaskStore> {
        match self {
            RemoteSnapshot::WholeFile(wf) => wf.store.as_ref(),
            RemoteSnapshot::CalDav(cd) => Some(&cd.store),
            RemoteSnapshot::MsTodo(ms) => Some(&ms.store),
            RemoteSnapshot::GoogleTasks(gt) => Some(&gt.store),
        }
    }
}

#[derive(Debug)]
pub(crate) enum PushError {
    Conflict,
    Other(String),
}

/// Compute a deterministic board cache path under `$XDG_STATE_HOME/roke/boards/<hash>/todos.jsonl`
/// for repos that define a project-local `.roke.toml` (e.g. a shared team remote tracker) without
/// committing a `.roke.jsonl` file.
pub fn project_cache_board_path(project_dir: &Path) -> PathBuf {
    let resolved = fs::canonicalize(project_dir).unwrap_or_else(|_| project_dir.to_path_buf());
    let hash = oauth::sha256(resolved.to_string_lossy().as_bytes());
    let short_hex: String = hash[..8].iter().map(|b| format!("{b:02x}")).collect();
    default_state_dir()
        .join("boards")
        .join(short_hex)
        .join("todos.jsonl")
}

/// Derive the XDG state directory for sync cursors (`$XDG_STATE_HOME/roke` or `~/.local/state/roke`).
/// The default global board stores cursors directly under the XDG state dir; project-local or
/// custom boards get an isolated subdirectory under `boards/<hash>/` so Git repos and Syncthing
/// folders are never polluted with machine-local sync state or OAuth tokens.
pub fn sync_state_dir_for(storage_path: Option<&Path>) -> PathBuf {
    let base = default_state_dir();
    let Some(path) = storage_path else {
        return base;
    };
    let default_path = default_storage_path();
    if path == default_path {
        return base;
    }
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let default_resolved = fs::canonicalize(&default_path).unwrap_or(default_path);
    if resolved == default_resolved {
        return base;
    }
    let hash = oauth::sha256(resolved.to_string_lossy().as_bytes());
    let short_hex: String = hash[..8].iter().map(|b| format!("{b:02x}")).collect();
    base.join("boards").join(short_hex)
}

/// Detect and automatically absorb Syncthing conflict files next to `storage_path`
/// (matching `<stem>.sync-conflict-*.<ext>`). Merges them via `merge_stores`, saves
/// `storage_path` atomically, and deletes the absorbed conflict files.
pub fn absorb_syncthing_conflicts(storage_path: &Path, local_name: &str) -> io::Result<usize> {
    let Some(parent) = storage_path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(0);
    };
    if !parent.is_dir() {
        return Ok(0);
    }
    let Some(file_name) = storage_path.file_name().and_then(|n| n.to_str()) else {
        return Ok(0);
    };
    let (stem, ext_suffix) = match file_name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (file_name, String::new()),
    };
    let prefix = format!("{stem}.sync-conflict-");

    let mut conflicts: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(parent)?.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str.starts_with(&prefix) && name_str.ends_with(&ext_suffix) {
            let p = entry.path();
            if p.is_file() {
                conflicts.push(p);
            }
        }
    }

    if conflicts.is_empty() {
        return Ok(0);
    }
    conflicts.sort();

    let _lock = FileLock::acquire(storage_path)?;
    let mut base = TaskStore::load_or_empty(storage_path)?;
    base.ensure_sync_metadata(local_name);

    let mut absorbed = 0usize;
    let mut to_remove = Vec::new();
    for conflict_path in conflicts {
        let Ok(raw) = fs::read_to_string(&conflict_path) else {
            continue;
        };
        if raw.trim().is_empty() {
            to_remove.push(conflict_path);
            absorbed += 1;
            continue;
        }
        if let Ok(mut other) = TaskStore::from_str_auto(&raw) {
            other.ensure_sync_metadata(local_name);
            base = merge_stores(&base, &other, local_name, local_name);
            to_remove.push(conflict_path);
            absorbed += 1;
        }
    }

    if absorbed > 0 {
        base.save(storage_path)?;
        for path in to_remove {
            let _ = fs::remove_file(path);
        }
    }

    Ok(absorbed)
}

/// Deterministic LWW-Element-Set merge between two stores.
/// Newer `updated_at` wins per task `uid`, and newer tombstones win over older task edits.
pub fn merge_stores(
    local: &TaskStore,
    remote: &TaskStore,
    local_origin_fallback: &str,
    remote_origin_fallback: &str,
) -> TaskStore {
    let mut tombstones = local.tombstones.clone();
    for (uid, &ts) in &remote.tombstones {
        tombstones
            .entry(uid.clone())
            .and_modify(|cur| {
                if ts > *cur {
                    *cur = ts;
                }
            })
            .or_insert(ts);
    }

    let mut order: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut by_uid: HashMap<String, Task> = HashMap::new();

    for task in &local.tasks {
        let mut t = task.clone();
        if t.origin.as_deref().unwrap_or("").is_empty() {
            t.origin = Some(normalize_device_tag(local_origin_fallback));
        }
        if seen.insert(t.uid.clone()) {
            order.push(t.uid.clone());
        }
        by_uid.insert(t.uid.clone(), t);
    }

    for task in &remote.tasks {
        let mut t = task.clone();
        if t.origin.as_deref().unwrap_or("").is_empty() {
            t.origin = Some(normalize_device_tag(remote_origin_fallback));
        }
        let uid = t.uid.clone();
        match by_uid.get(&uid) {
            Some(existing) => {
                if t.updated_at > existing.updated_at {
                    // Preserve original machine origin if remote had none
                    if t.origin.is_none() {
                        t.origin = existing.origin.clone();
                    }
                    by_uid.insert(uid, t);
                }
            }
            None => {
                if seen.insert(uid.clone()) {
                    order.push(uid.clone());
                }
                by_uid.insert(uid, t);
            }
        }
    }

    let mut merged_tasks = Vec::new();
    let mut used_ids: HashSet<u64> = HashSet::new();
    let mut next_id = local.next_id.max(remote.next_id).max(1);

    for uid in order {
        let Some(mut task) = by_uid.remove(&uid) else {
            continue;
        };
        if let Some(&deleted_at) = tombstones.get(&uid) {
            if deleted_at >= task.updated_at {
                continue;
            }
        }
        if task.id == 0 || !used_ids.insert(task.id) {
            task.id = next_id;
            next_id += 1;
            used_ids.insert(task.id);
        } else if task.id >= next_id {
            next_id = task.id + 1;
        }
        merged_tasks.push(task);
    }

    if merged_tasks.iter().any(|t| t.order.is_some()) {
        let mut indexed: Vec<(usize, Task)> = merged_tasks.into_iter().enumerate().collect();
        indexed.sort_by_key(|(idx, t)| (t.order.unwrap_or(i64::MAX), *idx));
        merged_tasks = indexed.into_iter().map(|(_, t)| t).collect();
    }

    TaskStore {
        next_id,
        tasks: merged_tasks,
        tombstones,
    }
}

fn stores_semantically_equal(a: &TaskStore, b: &TaskStore) -> bool {
    if a.tombstones != b.tombstones || a.tasks.len() != b.tasks.len() {
        return false;
    }
    let a_by_uid: HashMap<&str, &Task> = a.tasks.iter().map(|t| (t.uid.as_str(), t)).collect();
    b.tasks.iter().all(|bt| {
        a_by_uid.get(bt.uid.as_str()).is_some_and(|at| {
            at.title == bt.title
                && at.description == bt.description
                && at.status == bt.status
                && at.priority == bt.priority
                && at.tags == bt.tags
                && at.due == bt.due
                && at.due_time == bt.due_time
                && at.repeat == bt.repeat
                && at.created_at == bt.created_at
                && at.completed_at == bt.completed_at
                && at.updated_at == bt.updated_at
                && at.origin == bt.origin
                && at.order == bt.order
        })
    })
}

fn pull_peer(peer: &PeerConfig, state_dir: Option<&Path>) -> Result<RemoteSnapshot, String> {
    match peer.kind {
        PeerKind::Ssh => ssh::pull(peer).map(RemoteSnapshot::WholeFile),
        PeerKind::File => file::pull(peer).map(RemoteSnapshot::WholeFile),
        PeerKind::Webdav => webdav::pull(peer).map(RemoteSnapshot::WholeFile),
        PeerKind::Caldav => caldav::pull(peer, state_dir).map(RemoteSnapshot::CalDav),
        PeerKind::MsTodo => ms_todo::pull(peer, state_dir).map(RemoteSnapshot::MsTodo),
        PeerKind::GoogleTasks => {
            google_tasks::pull(peer, state_dir).map(RemoteSnapshot::GoogleTasks)
        }
    }
}

fn push_peer(
    peer: &PeerConfig,
    merged: &TaskStore,
    snapshot: &RemoteSnapshot,
    state_dir: Option<&Path>,
) -> Result<(), PushError> {
    match (peer.kind, snapshot) {
        (PeerKind::Ssh, RemoteSnapshot::WholeFile(wf)) => ssh::push(peer, merged, Some(&wf.token)),
        (PeerKind::File, RemoteSnapshot::WholeFile(wf)) => {
            file::push(peer, merged, Some(&wf.token))
        }
        (PeerKind::Webdav, RemoteSnapshot::WholeFile(wf)) => {
            webdav::push(peer, merged, Some(&wf.token))
        }
        (PeerKind::Caldav, RemoteSnapshot::CalDav(cd)) => caldav::push(peer, merged, cd),
        (PeerKind::MsTodo, RemoteSnapshot::MsTodo(ms)) => {
            ms_todo::push(peer, merged, ms, state_dir)
        }
        (PeerKind::GoogleTasks, RemoteSnapshot::GoogleTasks(gt)) => {
            google_tasks::push(peer, merged, gt, state_dir)
        }
        _ => Err(PushError::Other(format!(
            "internal snapshot mismatch for peer kind {}",
            peer.kind.label()
        ))),
    }
}

pub fn sync_all_peers(
    local_store: TaskStore,
    config: SyncConfig,
    state_dir: Option<PathBuf>,
) -> SyncOutcome {
    let local_name = config.effective_device_name();
    let mut working = local_store;
    working.ensure_sync_metadata(&local_name);

    let mut synced_peers = 0;
    let mut errors = Vec::new();
    let mut reachable_peers: Vec<(&PeerConfig, RemoteSnapshot)> = Vec::new();
    let state_dir_ref = state_dir.as_deref();

    // Pass 1: Pull from all peers in parallel
    let pull_results: Vec<(&PeerConfig, Result<RemoteSnapshot, String>)> =
        std::thread::scope(|scope| {
            let handles: Vec<_> = config
                .peers
                .iter()
                .map(|peer| scope.spawn(move || (peer, pull_peer(peer, state_dir_ref))))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("peer pull thread panicked"))
                .collect()
        });

    for (peer, result) in pull_results {
        match result {
            Ok(snapshot) => {
                if let Some(remote_store) = snapshot.remote_store() {
                    working = merge_stores(&working, remote_store, &local_name, &peer.name);
                }
                reachable_peers.push((peer, snapshot));
            }
            Err(err) => {
                errors.push(format!("{}: {err}", peer.name));
            }
        }
    }

    // Pass 2: Push merged store back in parallel with CAS, skipping whole-file peers already up to date.
    // Per-item backends (CalDAV, Microsoft To Do, Google Tasks) always run push (skipping unchanged
    // items internally and updating their seen-ID state file).
    let mut peers_to_push: Vec<(&PeerConfig, RemoteSnapshot)> = Vec::new();
    for (peer, snapshot) in reachable_peers {
        let skip_push = match &snapshot {
            RemoteSnapshot::WholeFile(wf) => match wf.store {
                Some(ref remote) => stores_semantically_equal(&working, remote),
                None => working.tasks.is_empty() && working.tombstones.is_empty(),
            },
            RemoteSnapshot::CalDav(_)
            | RemoteSnapshot::MsTodo(_)
            | RemoteSnapshot::GoogleTasks(_) => false,
        };
        if skip_push {
            synced_peers += 1;
        } else {
            peers_to_push.push((peer, snapshot));
        }
    }

    let working_ref = &working;
    let push_results: Vec<(&PeerConfig, Result<(), PushError>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = peers_to_push
            .into_iter()
            .map(|(peer, snapshot)| {
                scope.spawn(move || (peer, push_peer(peer, working_ref, &snapshot, state_dir_ref)))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("peer push thread panicked"))
            .collect()
    });

    for (peer, result) in push_results {
        match result {
            Ok(()) => synced_peers += 1,
            Err(PushError::Conflict) => {
                // Peer modified its state during sync: re-pull, re-merge, and retry CAS once
                match pull_peer(peer, state_dir_ref) {
                    Ok(latest) => {
                        if let Some(remote_store) = latest.remote_store() {
                            working = merge_stores(&working, remote_store, &local_name, &peer.name);
                        }
                        match push_peer(peer, &working, &latest, state_dir_ref) {
                            Ok(()) => synced_peers += 1,
                            Err(PushError::Conflict) => {
                                errors.push(format!("{}: remote changed concurrently", peer.name));
                            }
                            Err(PushError::Other(err)) => {
                                errors.push(format!("{}: {err}", peer.name));
                            }
                        }
                    }
                    Err(err) => errors.push(format!("{}: {err}", peer.name)),
                }
            }
            Err(PushError::Other(err)) => errors.push(format!("{}: {err}", peer.name)),
        }
    }

    SyncOutcome {
        merged_store: working,
        synced_peers,
        errors,
    }
}

pub struct SyncManager {
    tx: Sender<SyncOutcome>,
    rx: Receiver<SyncOutcome>,
    pub in_flight: bool,
}

impl SyncManager {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx,
            in_flight: false,
        }
    }

    pub fn trigger(
        &mut self,
        store: TaskStore,
        config: SyncConfig,
        state_dir: Option<PathBuf>,
    ) -> bool {
        if self.in_flight || config.peers.is_empty() {
            return false;
        }
        self.in_flight = true;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let outcome = sync_all_peers(store, config, state_dir);
            let _ = tx.send(outcome);
        });
        true
    }

    pub fn poll(&mut self) -> Option<SyncOutcome> {
        match self.rx.try_recv() {
            Ok(outcome) => {
                self.in_flight = false;
                Some(outcome)
            }
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Priority, Status};

    #[test]
    fn merge_combines_tasks_resolves_lww_and_respects_tombstones() {
        let mut local = TaskStore::new_empty();
        let id1 = local.add_with_origin(
            "Shared task".into(),
            "old".into(),
            Status::Todo,
            Priority::Medium,
            Vec::new(),
            "macbook",
        );
        let shared_uid = local.get(id1).unwrap().uid.clone();
        local.get_mut(id1).unwrap().updated_at = 100;

        let id_del = local.add_with_origin(
            "To be deleted".into(),
            String::new(),
            Status::Todo,
            Priority::Low,
            Vec::new(),
            "macbook",
        );
        let del_uid = local.get(id_del).unwrap().uid.clone();
        local.get_mut(id_del).unwrap().updated_at = 100;

        let mut remote = local.clone();
        remote.get_mut(id1).unwrap().description = "updated on vps".into();
        remote.get_mut(id1).unwrap().status = Status::InProgress;
        remote.get_mut(id1).unwrap().updated_at = 200;

        remote.add_with_origin(
            "From VPS".into(),
            String::new(),
            Status::Todo,
            Priority::High,
            vec!["ops".into()],
            "vps",
        );

        local.remove(id_del);
        local.tombstones.insert(del_uid.clone(), 250);

        let merged = merge_stores(&local, &remote, "macbook", "vps");
        assert_eq!(merged.tasks.len(), 2);

        let shared = merged.tasks.iter().find(|t| t.uid == shared_uid).unwrap();
        assert_eq!(shared.description, "updated on vps");
        assert_eq!(shared.status, Status::InProgress);

        let from_vps = merged.tasks.iter().find(|t| t.title == "From VPS").unwrap();
        assert_eq!(from_vps.origin.as_deref(), Some("vps"));
        assert!(!merged.tasks.iter().any(|t| t.uid == del_uid));
    }

    #[test]
    fn merge_propagates_task_reordering() {
        let mut local = TaskStore::new_empty();
        local.add_with_origin(
            "First".into(),
            String::new(),
            Status::Todo,
            Priority::Medium,
            Vec::new(),
            "mac",
        );
        local.add_with_origin(
            "Second".into(),
            String::new(),
            Status::Todo,
            Priority::Medium,
            Vec::new(),
            "mac",
        );
        local.ensure_orders();

        let mut remote = local.clone();
        remote.swap_order(0, 1);

        let merged = merge_stores(&local, &remote, "mac", "vps");
        let titles: Vec<&str> = merged.tasks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, vec!["Second", "First"]);
    }

    #[test]
    fn absorb_syncthing_conflicts_merges_and_cleans_up_files() {
        let dir = std::env::temp_dir().join(format!("roke_st_conflict_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let main_path = dir.join("todos.jsonl");
        let conflict_a = dir.join("todos.sync-conflict-20261127-090000-DEV1.jsonl");
        let conflict_b = dir.join("todos.sync-conflict-20261127-090500-DEV2.jsonl");
        let unrelated = dir.join("archive.jsonl");

        let mut s_main = TaskStore::new_empty();
        s_main.add_with_origin(
            "Main task".into(),
            String::new(),
            Status::Todo,
            Priority::Medium,
            Vec::new(),
            "mac",
        );
        s_main.save(&main_path).unwrap();

        let mut s_a = TaskStore::new_empty();
        s_a.add_with_origin(
            "Conflict A".into(),
            String::new(),
            Status::Todo,
            Priority::High,
            Vec::new(),
            "phone",
        );
        s_a.save(&conflict_a).unwrap();

        let mut s_b = TaskStore::new_empty();
        s_b.add_with_origin(
            "Conflict B".into(),
            String::new(),
            Status::Done,
            Priority::Low,
            Vec::new(),
            "linux",
        );
        s_b.save(&conflict_b).unwrap();

        fs::write(&unrelated, "{\"title\":\"Keep archive\"}\n").unwrap();

        let count = absorb_syncthing_conflicts(&main_path, "mac").unwrap();
        assert_eq!(count, 2);
        assert!(!conflict_a.exists());
        assert!(!conflict_b.exists());
        assert!(unrelated.exists());

        let merged = TaskStore::load_or_empty(&main_path).unwrap();
        assert_eq!(merged.tasks.len(), 3);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_state_dir_uses_xdg_state_and_isolates_project_boards() {
        let global_state = sync_state_dir_for(None);
        assert_eq!(global_state, default_state_dir());

        let default_board_state = sync_state_dir_for(Some(&default_storage_path()));
        assert_eq!(default_board_state, default_state_dir());

        let project_a = sync_state_dir_for(Some(Path::new("/tmp/project-a/.roke.jsonl")));
        let project_b = sync_state_dir_for(Some(Path::new("/tmp/project-b/.roke.jsonl")));
        assert!(project_a.starts_with(default_state_dir().join("boards")));
        assert!(project_b.starts_with(default_state_dir().join("boards")));
        assert_ne!(project_a, project_b);
    }
}
