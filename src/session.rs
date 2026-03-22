use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

/// Information about a discovered session.
pub struct SessionInfo {
    pub name: String,
    pub alive: bool,
}

/// Determine the socket directory using the platform fallback chain:
/// 1. $DTM_TMPDIR
/// 2. $XDG_RUNTIME_DIR/dtm/
/// 3. $TMPDIR/dtm-{uid}/
/// 4. /tmp/dtm-{uid}/
///
/// Creates the directory (mode 0700) if it does not exist.
pub fn socket_dir() -> PathBuf {
    let dir = if let Ok(d) = std::env::var("DTM_TMPDIR") {
        PathBuf::from(d)
    } else if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(d).join("dtm")
    } else {
        let uid = nix::unistd::getuid();
        if let Ok(d) = std::env::var("TMPDIR") {
            PathBuf::from(d).join(format!("dtm-{}", uid))
        } else {
            PathBuf::from(format!("/tmp/dtm-{}", uid))
        }
    };

    if !dir.exists() {
        let _ = fs::create_dir_all(&dir);
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }

    dir
}

/// Return the socket path for a named session.
pub fn socket_path(name: &str) -> PathBuf {
    socket_dir().join(format!("{}.sock", name))
}

/// List all sessions by scanning the socket directory.
/// Checks liveness via the PID file (kill(pid, 0)) to avoid connecting
/// to the socket, which would disrupt the active client.
pub fn list_sessions() -> Vec<SessionInfo> {
    let dir = socket_dir();
    let mut sessions = Vec::new();

    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return sessions,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sock") {
            continue;
        }
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let alive = is_alive(&name);
        sessions.push(SessionInfo { name, alive });
    }

    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    sessions
}

/// Check if a session's server process is still running via kill(pid, 0).
fn is_alive(name: &str) -> bool {
    match read_pid(name) {
        Some(pid) => {
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
        }
        None => false,
    }
}

/// Generate a default session name (sequential integer).
pub fn generate_name() -> String {
    let dir = socket_dir();
    let mut max: i64 = -1;

    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("sock") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if let Ok(n) = stem.parse::<i64>() {
                    if n > max {
                        max = n;
                    }
                }
            }
        }
    }

    ((max + 1) as u64).to_string()
}

/// Remove a stale socket file and its PID file.
pub fn cleanup_stale(name: &str) {
    let _ = fs::remove_file(socket_path(name));
    let _ = fs::remove_file(pid_path(name));
}

/// Return the PID file path for a named session.
pub fn pid_path(name: &str) -> PathBuf {
    socket_dir().join(format!("{}.pid", name))
}

/// Write the server PID to the PID file.
pub fn write_pid(name: &str, pid: u32) {
    let _ = fs::write(pid_path(name), pid.to_string());
}

/// Read the server PID from the PID file.
pub fn read_pid(name: &str) -> Option<i32> {
    fs::read_to_string(pid_path(name))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}
