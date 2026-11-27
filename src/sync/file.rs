use crate::config::{expand_tilde, PeerConfig};
use crate::model::{FileLock, TaskStore};
use crate::sync::ssh::posix_cksum;
use crate::sync::{absorb_syncthing_conflicts, PushError, WholeFileSnapshot};
use std::fs;
use std::path::PathBuf;

fn peer_file_path(peer: &PeerConfig) -> Result<PathBuf, String> {
    let raw = peer
        .path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing 'path' for file peer".to_string())?;
    Ok(expand_tilde(raw))
}

pub(crate) fn pull(peer: &PeerConfig) -> Result<WholeFileSnapshot, String> {
    let path = peer_file_path(peer)?;
    // If Syncthing or another folder sync tool left conflict siblings next to the peer file,
    // absorb them cleanly before reading.
    let _ = absorb_syncthing_conflicts(&path, &peer.name);

    if !path.exists() {
        return Ok(WholeFileSnapshot {
            store: None,
            token: "missing".to_string(),
        });
    }

    let _lock =
        FileLock::acquire(&path).map_err(|e| format!("failed to lock {}: {e}", path.display()))?;
    if !path.exists() {
        return Ok(WholeFileSnapshot {
            store: None,
            token: "missing".to_string(),
        });
    }
    let bytes = fs::read(&path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let cksum = posix_cksum(&bytes);
    let raw = String::from_utf8_lossy(&bytes);
    if raw.trim().is_empty() {
        return Ok(WholeFileSnapshot {
            store: None,
            token: cksum,
        });
    }
    let mut store = TaskStore::from_str_auto(&raw)
        .map_err(|e| format!("invalid peer task file {}: {e}", path.display()))?;
    store.ensure_sync_metadata(&peer.name);
    Ok(WholeFileSnapshot {
        store: Some(store),
        token: cksum,
    })
}

pub(crate) fn push(
    peer: &PeerConfig,
    store: &TaskStore,
    expected_cksum: Option<&str>,
) -> Result<(), PushError> {
    let path = peer_file_path(peer).map_err(PushError::Other)?;
    let _lock = FileLock::acquire(&path)
        .map_err(|e| PushError::Other(format!("failed to lock {}: {e}", path.display())))?;

    if let Some(expected) = expected_cksum {
        let current = if path.exists() {
            let bytes = fs::read(&path)
                .map_err(|e| PushError::Other(format!("failed to read {}: {e}", path.display())))?;
            posix_cksum(&bytes)
        } else {
            "missing".to_string()
        };
        if current != expected {
            return Err(PushError::Conflict);
        }
    }

    store
        .save(&path)
        .map_err(|e| PushError::Other(format!("failed to save {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PeerKind, SyncConfig};
    use crate::model::{Priority, Status};
    use crate::sync::sync_all_peers;

    fn test_peer(name: &str, path: PathBuf) -> PeerConfig {
        PeerConfig {
            name: name.into(),
            kind: PeerKind::File,
            host: String::new(),
            remote_path: None,
            port: None,
            identity_file: None,
            url: None,
            list: None,
            path: Some(path.to_string_lossy().to_string()),
            username: None,
            password: None,
            password_env: None,
            password_cmd: None,
            inject_device_tag: None,
        }
    }

    #[test]
    fn file_peer_two_way_sync_and_syncthing_conflict_merge() {
        let dir = std::env::temp_dir().join(format!("roke_file_sync_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let peer_file = dir.join("todos.jsonl");
        let conflict_file = dir.join("todos.sync-conflict-20261127-100000-ABCDEF1.jsonl");

        let mut peer_store = TaskStore::new_empty();
        peer_store.add_with_origin(
            "From syncthing".into(),
            String::new(),
            Status::Todo,
            Priority::High,
            vec!["sync".into()],
            "phone",
        );
        peer_store.save(&peer_file).unwrap();

        let mut conflict_store = TaskStore::new_empty();
        conflict_store.add_with_origin(
            "From offline conflict".into(),
            String::new(),
            Status::InProgress,
            Priority::Medium,
            vec!["conflict".into()],
            "tablet",
        );
        conflict_store.save(&conflict_file).unwrap();

        let mut local = TaskStore::new_empty();
        local.add_with_origin(
            "From laptop".into(),
            String::new(),
            Status::Todo,
            Priority::Low,
            Vec::new(),
            "laptop",
        );

        let cfg = SyncConfig {
            device_name: Some("laptop".into()),
            peers: vec![test_peer("syncthing", peer_file.clone())],
            ..SyncConfig::default()
        };

        let outcome = sync_all_peers(local, cfg, Some(dir.join(".state")));
        assert!(outcome.errors.is_empty(), "errors: {:?}", outcome.errors);
        assert_eq!(outcome.synced_peers, 1);
        assert_eq!(outcome.merged_store.tasks.len(), 3);
        assert!(
            !conflict_file.exists(),
            "Syncthing conflict file should be absorbed and deleted"
        );

        let persisted = TaskStore::load_or_empty(&peer_file).unwrap();
        assert_eq!(persisted.tasks.len(), 3);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_peer_detects_cas_conflict() {
        let dir = std::env::temp_dir().join(format!("roke_file_cas_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let peer_file = dir.join("todos.jsonl");

        let mut store = TaskStore::new_empty();
        store.add(
            "Initial".into(),
            String::new(),
            Status::Todo,
            Priority::Medium,
            Vec::new(),
        );
        store.save(&peer_file).unwrap();

        let peer = test_peer("syncthing", peer_file.clone());
        let pulled = pull(&peer).unwrap();

        // Concurrent modification on disk
        let mut modified = store.clone();
        modified.add(
            "Concurrent".into(),
            String::new(),
            Status::Todo,
            Priority::High,
            Vec::new(),
        );
        modified.save(&peer_file).unwrap();

        let res = push(&peer, &store, Some(&pulled.token));
        assert!(matches!(res, Err(PushError::Conflict)));

        let _ = fs::remove_dir_all(&dir);
    }
}
