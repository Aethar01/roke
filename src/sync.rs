use crate::config::{normalize_device_tag, PeerConfig, SyncConfig};
use crate::model::{Task, TaskStore};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

#[derive(Debug, Clone)]
pub struct SyncOutcome {
    pub merged_store: TaskStore,
    pub synced_peers: usize,
    pub errors: Vec<String>,
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

    TaskStore {
        next_id,
        tasks: merged_tasks,
        tombstones,
    }
}

fn ssh_base_args(peer: &PeerConfig) -> Vec<String> {
    let mut args = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=8".into(),
    ];
    if let Some(port) = peer.port {
        args.push("-p".into());
        args.push(port.to_string());
    }
    if let Some(identity) = &peer.identity_file {
        if !identity.trim().is_empty() {
            args.push("-i".into());
            args.push(identity.clone());
        }
    }
    args
}

fn remote_path_arg(remote_path: Option<&str>) -> String {
    match remote_path.map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => {
            // Single-quoted argument safe across POSIX sh and fish when no single-quotes/backslashes.
            let safe = p.replace('\\', "\\\\").replace('\'', "\\'");
            format!(" '{safe}'")
        }
        None => String::new(),
    }
}

fn format_ssh_stderr(output: &std::process::Output) -> String {
    let compact = String::from_utf8_lossy(&output.stderr)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if compact.is_empty() {
        format!("ssh exited with {}", output.status)
    } else {
        compact
    }
}

/// Runs a POSIX sh snippet via `sh -c '...'` with zero backslashes or single quotes inside the
/// body so it works regardless of whether the remote login shell is bash, zsh, sh, or fish.
fn build_pull_command(remote_path: Option<&str>) -> String {
    let body = r#"p="${1:-${XDG_DATA_HOME:-$HOME/.local/share}/roke/todos.jsonl}"; case "$p" in "~/"*) p="$HOME/${p#~/}" ;; esac; [ -f "$p" ] && cat "$p" || true"#;
    format!("sh -c '{body}' _{}", remote_path_arg(remote_path))
}

fn build_push_command(remote_path: Option<&str>) -> String {
    let body = r#"p="${1:-${XDG_DATA_HOME:-$HOME/.local/share}/roke/todos.jsonl}"; case "$p" in "~/"*) p="$HOME/${p#~/}" ;; esac; mkdir -p "$(dirname "$p")" && tmp="$p.tmp.$$" && cat > "$tmp" && mv "$tmp" "$p""#;
    format!("sh -c '{body}' _{}", remote_path_arg(remote_path))
}

fn pull_peer_store(peer: &PeerConfig) -> Result<Option<TaskStore>, String> {
    let script = build_pull_command(peer.remote_path.as_deref());

    let mut cmd = Command::new("ssh");
    cmd.args(ssh_base_args(peer)).arg(&peer.host).arg(script);

    let output = cmd
        .output()
        .map_err(|e| format!("failed to run ssh: {e}"))?;
    if !output.status.success() {
        return Err(format_ssh_stderr(&output));
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let mut store =
        TaskStore::from_str_auto(&raw).map_err(|e| format!("invalid remote task file: {e}"))?;
    store.ensure_sync_metadata(&peer.name);
    Ok(Some(store))
}

fn push_peer_store(peer: &PeerConfig, store: &TaskStore) -> Result<(), String> {
    let payload = store.to_jsonl().map_err(|e| e.to_string())?;
    let script = build_push_command(peer.remote_path.as_deref());

    let mut cmd = Command::new("ssh");
    cmd.args(ssh_base_args(peer))
        .arg(&peer.host)
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to run ssh: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(payload.as_bytes())
            .map_err(|e| format!("failed writing payload: {e}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("ssh wait error: {e}"))?;
    if !output.status.success() {
        return Err(format_ssh_stderr(&output));
    }
    Ok(())
}

pub fn sync_all_peers(local_store: TaskStore, config: SyncConfig) -> SyncOutcome {
    let local_name = config.effective_device_name();
    let mut working = local_store;
    working.ensure_sync_metadata(&local_name);

    let mut synced_peers = 0;
    let mut errors = Vec::new();
    let mut reachable_peers: Vec<&PeerConfig> = Vec::new();

    // Pass 1: Pull and merge from every reachable peer
    for peer in &config.peers {
        match pull_peer_store(peer) {
            Ok(Some(remote_store)) => {
                working = merge_stores(&working, &remote_store, &local_name, &peer.name);
                reachable_peers.push(peer);
            }
            Ok(None) => {
                reachable_peers.push(peer);
            }
            Err(err) => {
                errors.push(format!("{}: {err}", peer.name));
            }
        }
    }

    // Pass 2: Push the merged store back to every reachable peer
    for peer in reachable_peers {
        match push_peer_store(peer, &working) {
            Ok(()) => synced_peers += 1,
            Err(err) => errors.push(format!("{}: {err}", peer.name)),
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

    pub fn trigger(&mut self, store: TaskStore, config: SyncConfig) -> bool {
        if self.in_flight || config.peers.is_empty() {
            return false;
        }
        self.in_flight = true;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let outcome = sync_all_peers(store, config);
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
        // Remote updates Shared task with newer timestamp
        remote.get_mut(id1).unwrap().description = "updated on vps".into();
        remote.get_mut(id1).unwrap().status = Status::InProgress;
        remote.get_mut(id1).unwrap().updated_at = 200;

        // Remote adds a brand new task
        remote.add_with_origin(
            "From VPS".into(),
            String::new(),
            Status::Todo,
            Priority::High,
            vec!["ops".into()],
            "vps",
        );

        // Local deletes "To be deleted" at timestamp 250
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
    fn remote_shell_commands_work_under_sh_and_fish() {
        let dir = std::env::temp_dir().join(format!("roke_sync_sh_{}", std::process::id()));
        let file = dir.join("todos.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        let file_str = file.to_string_lossy().to_string();

        let push_cmd = build_push_command(Some(&file_str));
        let pull_cmd = build_pull_command(Some(&file_str));

        // Ensure command bodies contain no backslashes or nested quotes that trip up fish
        assert!(push_cmd.starts_with("sh -c '"));
        assert!(pull_cmd.starts_with("sh -c '"));

        // Run push + pull through sh (and fish if installed)
        let mut shells = vec!["sh"];
        if Command::new("fish").arg("--version").output().is_ok() {
            shells.push("fish");
        }

        for shell in shells {
            let mut child = Command::new(shell)
                .arg("-c")
                .arg(&push_cmd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn shell");
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"{\"title\":\"hello\"}\n")
                .unwrap();
            let push_out = child.wait_with_output().unwrap();
            assert!(
                push_out.status.success(),
                "{shell} push failed: {}",
                String::from_utf8_lossy(&push_out.stderr)
            );

            let pull_out = Command::new(shell)
                .arg("-c")
                .arg(&pull_cmd)
                .output()
                .unwrap();
            assert!(
                pull_out.status.success(),
                "{shell} pull failed: {}",
                String::from_utf8_lossy(&pull_out.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&pull_out.stdout),
                "{\"title\":\"hello\"}\n"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
