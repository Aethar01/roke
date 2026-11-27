use crate::config::PeerConfig;
use crate::model::TaskStore;
use crate::sync::http::{
    apply_auth, basic_auth_header, build_agent, normalize_etag, read_response_string,
};
use crate::sync::{PushError, WholeFileSnapshot};

fn peer_url(peer: &PeerConfig) -> Result<&str, String> {
    peer.url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing 'url' for webdav peer".to_string())
}

pub(crate) fn pull(peer: &PeerConfig) -> Result<WholeFileSnapshot, String> {
    let url = peer_url(peer)?;
    let auth = basic_auth_header(peer)?;
    let agent = build_agent();

    let req = apply_auth(agent.get(url), auth.as_deref());
    match req.call() {
        Ok(resp) => {
            let token = resp
                .header("ETag")
                .map(|e| format!("etag:{}", normalize_etag(e)))
                .unwrap_or_else(|| "no-etag".to_string());
            let body = read_response_string(resp)?;
            if body.trim().is_empty() {
                return Ok(WholeFileSnapshot { store: None, token });
            }
            let mut store = TaskStore::from_str_auto(&body)
                .map_err(|e| format!("invalid remote task file: {e}"))?;
            store.ensure_sync_metadata(&peer.name);
            Ok(WholeFileSnapshot {
                store: Some(store),
                token,
            })
        }
        Err(ureq::Error::Status(404, _)) => Ok(WholeFileSnapshot {
            store: None,
            token: "missing".to_string(),
        }),
        Err(ureq::Error::Status(code, resp)) => {
            let status_text = resp.status_text().to_string();
            Err(format!("WebDAV GET failed: HTTP {code} {status_text}"))
        }
        Err(ureq::Error::Transport(t)) => Err(format!("WebDAV request failed: {t}")),
    }
}

pub(crate) fn push(
    peer: &PeerConfig,
    store: &TaskStore,
    expected_token: Option<&str>,
) -> Result<(), PushError> {
    let url = peer_url(peer).map_err(PushError::Other)?;
    let auth = basic_auth_header(peer).map_err(PushError::Other)?;
    let payload = store
        .to_jsonl()
        .map_err(|e| PushError::Other(e.to_string()))?;
    let agent = build_agent();

    let mut req = apply_auth(
        agent
            .put(url)
            .set("Content-Type", "application/x-ndjson; charset=utf-8"),
        auth.as_deref(),
    );
    match expected_token {
        Some("missing") => {
            req = req.set("If-None-Match", "*");
        }
        Some(tok) if tok.starts_with("etag:") => {
            let etag = &tok["etag:".len()..];
            if !etag.is_empty() {
                req = req.set("If-Match", etag);
            }
        }
        _ => {}
    }

    match req.send_string(&payload) {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(412, _)) => Err(PushError::Conflict),
        Err(ureq::Error::Status(code, resp)) => {
            let status_text = resp.status_text().to_string();
            Err(PushError::Other(format!(
                "WebDAV PUT failed: HTTP {code} {status_text}"
            )))
        }
        Err(ureq::Error::Transport(t)) => {
            Err(PushError::Other(format!("WebDAV request failed: {t}")))
        }
    }
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

    struct MockWebDavState {
        body: Option<String>,
        etag: String,
        inject_conflict_once: bool,
    }

    fn spawn_mock_webdav(state: Arc<Mutex<MockWebDavState>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut req_line = String::new();
                if reader.read_line(&mut req_line).is_err() || req_line.is_empty() {
                    continue;
                }
                let method = req_line.split_whitespace().next().unwrap_or("");
                let mut content_len = 0usize;
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
                match method {
                    "GET" => match &s.body {
                        Some(data) => {
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nETag: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                s.etag,
                                data.len(),
                                data
                            );
                            let _ = stream.write_all(resp.as_bytes());
                        }
                        None => {
                            let _ = stream.write_all(
                                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            );
                        }
                    },
                    "PUT" => {
                        if s.inject_conflict_once {
                            s.inject_conflict_once = false;
                            // Simulate a concurrent remote edit
                            let mut remote =
                                TaskStore::from_str_auto(s.body.as_deref().unwrap_or("")).unwrap();
                            remote.add_with_origin(
                                "Concurrent remote edit".into(),
                                String::new(),
                                Status::Todo,
                                Priority::High,
                                Vec::new(),
                                "cloud",
                            );
                            s.body = Some(remote.to_jsonl().unwrap());
                            s.etag = "\"v2\"".into();
                            let _ = stream.write_all(
                                b"HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            );
                        } else if if_none_match.as_deref() == Some("*") && s.body.is_some() {
                            let _ = stream.write_all(
                                b"HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            );
                        } else if let Some(ref expected) = if_match {
                            if expected != &s.etag {
                                let _ = stream.write_all(
                                    b"HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                );
                                continue;
                            }
                            s.body = Some(body_str);
                            s.etag = "\"v3\"".into();
                            let _ = stream.write_all(
                                b"HTTP/1.1 204 No Content\r\nETag: \"v3\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            );
                        } else {
                            s.body = Some(body_str);
                            s.etag = "\"v1\"".into();
                            let _ = stream.write_all(
                                b"HTTP/1.1 201 Created\r\nETag: \"v1\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            );
                        }
                    }
                    _ => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                }
            }
        });
        format!("http://{addr}/roke/todos.jsonl")
    }

    #[test]
    fn webdav_sync_handles_create_merge_and_412_cas_retry() {
        let mut initial = TaskStore::new_empty();
        initial.add_with_origin(
            "On server".into(),
            String::new(),
            Status::Todo,
            Priority::Medium,
            Vec::new(),
            "cloud",
        );

        let state = Arc::new(Mutex::new(MockWebDavState {
            body: Some(initial.to_jsonl().unwrap()),
            etag: "\"v1\"".into(),
            inject_conflict_once: true,
        }));
        let url = spawn_mock_webdav(Arc::clone(&state));

        let mut local = TaskStore::new_empty();
        local.add_with_origin(
            "From laptop".into(),
            String::new(),
            Status::Todo,
            Priority::High,
            Vec::new(),
            "laptop",
        );

        let cfg = SyncConfig {
            device_name: Some("laptop".into()),
            peers: vec![PeerConfig {
                name: "cloud".into(),
                kind: PeerKind::Webdav,
                host: String::new(),
                remote_path: None,
                port: None,
                identity_file: None,
                url: Some(url),
                list: None,
                path: None,
                username: Some("elliott".into()),
                password: Some("secret".into()),
                password_env: None,
                password_cmd: None,
                inject_device_tag: None,
            }],
            ..SyncConfig::default()
        };

        let outcome = sync_all_peers(local, cfg, None);
        assert!(outcome.errors.is_empty(), "errors: {:?}", outcome.errors);
        assert_eq!(outcome.synced_peers, 1);
        // Should contain all 3 tasks: initial server + local + concurrent edit recovered on 412 retry!
        assert_eq!(outcome.merged_store.tasks.len(), 3);

        let final_remote =
            TaskStore::from_str_auto(state.lock().unwrap().body.as_deref().unwrap()).unwrap();
        assert_eq!(final_remote.tasks.len(), 3);
    }
}
