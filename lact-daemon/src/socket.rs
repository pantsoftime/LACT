use anyhow::{Context, anyhow};
use nix::{
    sys::stat::{Mode, umask},
    unistd::{Gid, Group, User, chown, getuid},
};
use std::{
    env, fs,
    path::{Path, PathBuf},
    str::FromStr,
};
use tokio::net::UnixListener;
use tracing::{debug, info, warn};

use crate::{
    config,
    system::{IS_FLATBOX, run_command},
};

pub fn get_socket_path() -> PathBuf {
    let uid = getuid();

    if let Ok(path) = env::var("LACT_DAEMON_SOCKET_PATH") {
        PathBuf::from_str(&path).unwrap()
    } else if uid.is_root() {
        PathBuf::from_str("/run/lactd.sock").unwrap()
    } else {
        PathBuf::from_str(&format!("/run/user/{uid}/lactd.sock")).unwrap()
    }
}

pub fn cleanup() {
    let socket_path = get_socket_path();

    if socket_path.exists() {
        fs::remove_file(socket_path).expect("failed to remove socket");
    }
    debug!("removed socket");
}

pub fn listen() -> anyhow::Result<(UnixListener, PathBuf)> {
    let socket_path = get_socket_path();

    claim_socket_path(&socket_path)?;

    let socket_mask = Mode::S_IXUSR | Mode::S_IXGRP | Mode::S_IRWXO;
    umask(socket_mask);

    let listener = UnixListener::bind(&socket_path)?;

    info!("listening on {socket_path:?}");
    Ok((listener, socket_path))
}

/// Make `path` free for a new listener. A socket left behind by a daemon
/// that was killed or crashed (SIGKILL, OOM, a panic that skipped cleanup)
/// used to block every restart, so systemd's `Restart=on-failure` hit the
/// start limit and the daemon stayed down. A live daemon accepts a
/// connection; a stale socket refuses it, and only then is it removed.
fn claim_socket_path(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => Err(anyhow!(
            "Socket {} already exists and another instance of lact-daemon is answering on it",
            path.display()
        )),
        Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => {
            warn!(
                "removing stale socket {} (nothing is listening; the previous daemon did not shut down cleanly)",
                path.display()
            );
            fs::remove_file(path)
                .with_context(|| format!("Could not remove stale socket {}", path.display()))
        }
        Err(err) => Err(anyhow!(
            "Socket {} already exists and could not be probed ({err}). \
            If no other instance of lact-daemon is running, please remove the file",
            path.display()
        )),
    }
}

pub async fn set_permissions(
    socket_path: &Path,
    daemon_config: &config::Daemon,
) -> anyhow::Result<()> {
    let group = daemon_config
        .admin_group
        .as_ref()
        .map(|name| {
            Group::from_name(name)
                .context("Could not get group")?
                .with_context(|| format!("Group {name} does not exist"))
        })
        .transpose()?
        .map_or_else(Gid::current, |group| group.gid);

    let user = daemon_config
        .admin_user
        .as_ref()
        .map(|name| {
            User::from_name(name)
                .context("Could not get group")?
                .with_context(|| format!("Group {name} does not exist"))
        })
        .transpose()?
        .map(|user| user.uid);

    debug!("using gid {group} uid {user:?} for socket");

    if *IS_FLATBOX {
        let owner_arg = match user {
            Some(user) => format!("{}:{}", user.as_raw(), group.as_raw()),
            None => format!(":{}", group.as_raw()),
        };

        let path = socket_path
            .to_str()
            .expect("Invalid socket path")
            .trim_start_matches("/run/host/root");

        run_command("chown", &[&owner_arg, path])
            .await
            .context("Could not set socket permissions")?;
    } else {
        chown(socket_path, user, Some(group)).context("Could not set socket permissions")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::claim_socket_path;
    use std::os::unix::net::UnixListener;

    #[test]
    fn missing_path_is_free() {
        let dir = tempfile::tempdir().unwrap();
        claim_socket_path(&dir.path().join("lactd.sock")).unwrap();
    }

    #[test]
    fn stale_socket_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lactd.sock");
        drop(UnixListener::bind(&path).unwrap()); // file stays, nobody listens
        assert!(path.exists());
        claim_socket_path(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn live_socket_is_refused_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lactd.sock");
        let _live = UnixListener::bind(&path).unwrap();
        assert!(claim_socket_path(&path).is_err());
        assert!(path.exists());
    }
}
