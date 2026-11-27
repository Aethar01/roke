use crate::config::PeerConfig;
use crate::model::TaskStore;
use crate::sync::{PushError, WholeFileSnapshot};
use std::io::Write;
use std::process::{Command, Stdio};

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

fn needs_encoded_remote_path(path: &str) -> bool {
    path.contains('\'') || path.contains('\\') || path.contains('\n') || path.contains('\r')
}

fn encode_decimal_bytes(path: &str) -> String {
    let mut out = String::with_capacity(path.len() * 3);
    for b in path.as_bytes() {
        out.push_str(&format!("{b:03}"));
    }
    out
}

fn remote_path_arg(remote_path: Option<&str>) -> (bool, String) {
    match remote_path.map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) if needs_encoded_remote_path(p) => (true, format!(" {}", encode_decimal_bytes(p))),
        Some(p) => (false, format!(" '{p}'")),
        None => (false, String::new()),
    }
}

fn resolve_path_snippet(encoded: bool) -> &'static str {
    if encoded {
        r#"p=$(awk "BEGIN{s=ARGV[1];f=ARGV[2];for(i=1;i<=length(s);i+=3)printf f,substr(s,i,3)+0}" "$1" "%c"); case "$p" in "~/"*) p="$HOME/${p#~/}" ;; esac"#
    } else {
        r#"p="${1:-${XDG_DATA_HOME:-$HOME/.local/share}/roke/todos.jsonl}"; case "$p" in "~/"*) p="$HOME/${p#~/}" ;; esac"#
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

/// Compute POSIX `cksum` (`"<crc> <byte_len>"`) matching `cksum < file` on macOS, Linux, and BSD.
pub(crate) fn posix_cksum(bytes: &[u8]) -> String {
    let mut crc: u32 = 0;
    for &b in bytes {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            if (crc & 0x8000_0000) != 0 {
                crc = (crc << 1) ^ 0x04c1_1db7;
            } else {
                crc <<= 1;
            }
        }
    }
    let mut len = bytes.len();
    while len > 0 {
        let b = (len & 0xff) as u32;
        crc ^= b << 24;
        for _ in 0..8 {
            if (crc & 0x8000_0000) != 0 {
                crc = (crc << 1) ^ 0x04c1_1db7;
            } else {
                crc <<= 1;
            }
        }
        len >>= 8;
    }
    crc = !crc;
    format!("{crc} {}", bytes.len())
}

/// Runs a POSIX sh snippet via `sh -c '...'` with zero backslashes or single quotes inside the
/// body so it works regardless of whether the remote login shell is bash, zsh, sh, or fish.
fn build_pull_command(remote_path: Option<&str>) -> String {
    let (encoded, arg) = remote_path_arg(remote_path);
    let resolve = resolve_path_snippet(encoded);
    format!(r#"sh -c '{resolve}; [ -f "$p" ] && cat "$p" || true' _{arg}"#)
}

#[cfg(test)]
fn build_push_command(remote_path: Option<&str>) -> String {
    build_push_command_cas(remote_path, None)
}

fn build_push_command_cas(remote_path: Option<&str>, expected_cksum: Option<&str>) -> String {
    let (encoded, arg) = remote_path_arg(remote_path);
    let resolve = resolve_path_snippet(encoded);
    let cas_check = match expected_cksum {
        Some("missing") => r#"if [ -f "$p" ]; then exit 42; fi;"#.to_string(),
        Some(exp) => format!(
            r#"if [ ! -f "$p" ]; then exit 42; fi; set -- $(cksum < "$p"); if [ "$1 $2" != "{exp}" ]; then exit 42; fi;"#
        ),
        None => String::new(),
    };
    format!(
        r#"sh -c '{resolve}; mkdir -p "$(dirname "$p")" || exit 1; tmp="$p.tmp.$$"; cleanup() {{ rm -f "$tmp"; }}; trap cleanup EXIT HUP INT TERM; cat > "$tmp" || exit 1; {cas_check} mv "$tmp" "$p"' _{arg}"#
    )
}

pub(crate) fn pull(peer: &PeerConfig) -> Result<WholeFileSnapshot, String> {
    if peer.host.trim().is_empty() {
        return Err("missing 'host' for ssh peer".into());
    }
    let script = build_pull_command(peer.remote_path.as_deref());

    let mut cmd = Command::new("ssh");
    cmd.args(ssh_base_args(peer)).arg(&peer.host).arg(script);

    let output = cmd
        .output()
        .map_err(|e| format!("failed to run ssh: {e}"))?;
    if !output.status.success() {
        return Err(format_ssh_stderr(&output));
    }

    if output.stdout.is_empty() {
        return Ok(WholeFileSnapshot {
            store: None,
            token: "missing".to_string(),
        });
    }
    let cksum = posix_cksum(&output.stdout);
    let raw = String::from_utf8_lossy(&output.stdout);
    if raw.trim().is_empty() {
        return Ok(WholeFileSnapshot {
            store: None,
            token: cksum,
        });
    }
    let mut store =
        TaskStore::from_str_auto(&raw).map_err(|e| format!("invalid remote task file: {e}"))?;
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
    if peer.host.trim().is_empty() {
        return Err(PushError::Other("missing 'host' for ssh peer".into()));
    }
    let payload = store
        .to_jsonl()
        .map_err(|e| PushError::Other(e.to_string()))?;
    let script = build_push_command_cas(peer.remote_path.as_deref(), expected_cksum);

    let mut cmd = Command::new("ssh");
    cmd.args(ssh_base_args(peer))
        .arg(&peer.host)
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| PushError::Other(format!("failed to run ssh: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(payload.as_bytes())
            .map_err(|e| PushError::Other(format!("failed writing payload: {e}")))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| PushError::Other(format!("ssh wait error: {e}")))?;
    if output.status.code() == Some(42) {
        return Err(PushError::Conflict);
    }
    if !output.status.success() {
        return Err(PushError::Other(format_ssh_stderr(&output)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_shell_commands_work_under_sh_and_fish() {
        let dir = std::env::temp_dir().join(format!("roke_sync_sh_{}", std::process::id()));
        let file = dir.join("todos.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        let file_str = file.to_string_lossy().to_string();

        let push_cmd = build_push_command(Some(&file_str));
        let pull_cmd = build_pull_command(Some(&file_str));

        assert!(push_cmd.starts_with("sh -c '"));
        assert!(pull_cmd.starts_with("sh -c '"));

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

    #[test]
    fn remote_shell_commands_handle_quotes_and_spaces_under_sh_and_fish() {
        let dir =
            std::env::temp_dir().join(format!("roke_sync_quote's dir_{}", std::process::id()));
        let file = dir.join("my'todos\\list.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        let file_str = file.to_string_lossy().to_string();

        let push_cmd = build_push_command(Some(&file_str));
        let pull_cmd = build_pull_command(Some(&file_str));
        assert!(push_cmd.starts_with("sh -c '"));
        assert!(pull_cmd.starts_with("sh -c '"));

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
                .write_all(b"{\"title\":\"quoted\"}\n")
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
                "{\"title\":\"quoted\"}\n"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cas_push_detects_concurrent_change_and_cleans_temp_file() {
        let dir = std::env::temp_dir().join(format!("roke_sync_cas_{}", std::process::id()));
        let file = dir.join("todos.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let initial = b"{\"title\":\"initial\"}\n";
        std::fs::write(&file, initial).unwrap();
        let expected = posix_cksum(initial);
        let file_str = file.to_string_lossy().to_string();

        let ok_cmd = build_push_command_cas(Some(&file_str), Some(&expected));
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&ok_cmd)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"{\"title\":\"updated\"}\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "{\"title\":\"updated\"}\n"
        );

        let conflict_cmd = build_push_command_cas(Some(&file_str), Some(&expected));
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&conflict_cmd)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"{\"title\":\"clobber\"}\n")
            .unwrap();
        let conflict_out = child.wait_with_output().unwrap();
        assert_eq!(conflict_out.status.code(), Some(42));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "{\"title\":\"updated\"}\n"
        );
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "temp file must be cleaned up on CAS conflict"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
