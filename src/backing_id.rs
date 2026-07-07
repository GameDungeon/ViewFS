use std::{
    collections::HashSet,
    ffi::OsStr,
    fs::OpenOptions,
    io,
    os::{
        fd::{AsFd, AsRawFd, OwnedFd},
        unix::{ffi::OsStrExt, fs::OpenOptionsExt, net::UnixDatagram},
    },
    path::{Path, PathBuf},
    time::Duration,
};

use nix::libc;

use fuser::BackingId;
use log::{error, warn};
use nix::{
    ioctl_write_ptr,
    sys::wait::{waitpid, WaitPidFlag, WaitStatus},
    unistd::Pid,
};

use crate::ipc;

ioctl_write_ptr!(backing_close, 229u8, 2u8, u32);

pub(crate) fn backing_id_server(socket: UnixDatagram, child_pid: Pid) {
    let allowed = match ipc::recv_whitelist(&socket) {
        Ok(allowed) => allowed,
        Err(e) => {
            error!("[SERVER] failed to receive whitelist: {e}");
            return;
        }
    };

    let fuse_fd = match ipc::recv_fuse_fd(&socket) {
        Ok(fd) => fd,
        Err(e) => {
            error!("[SERVER] failed to receive FUSE fd: {e}");
            return;
        }
    };

    run_event_loop(&socket, &fuse_fd, &allowed, child_pid);
}

fn run_event_loop(
    socket: &UnixDatagram,
    fuse_fd: &OwnedFd,
    allowed: &HashSet<PathBuf>,
    child_pid: Pid,
) {
    // Set a 1-second read timeout so we can periodically check if the child has exited
    socket.set_read_timeout(Some(Duration::from_secs(1))).ok();
    let mut buf = vec![0u8; 8192];

    loop {
        let sz = match socket.recv(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::Exited(_, _)) | Ok(WaitStatus::Signaled(_, _, _)) => return,
                    Ok(_) => continue,
                    Err(_) => return,
                }
            }
            Err(e) => {
                error!("[SERVER] recv error (child likely gone): {e}");
                return;
            }
        };

        match ipc::Tag::try_from(buf[0]) {
            Ok(ipc::Tag::Open) => handle_open(socket, fuse_fd, allowed, &buf[1..sz]),
            Ok(ipc::Tag::Close) => handle_close(fuse_fd, &buf[1..sz]),
            Err(tag) => warn!("[SERVER] unknown message tag: {tag}"),
        }
    }
}

fn handle_open(socket: &UnixDatagram, fuse_fd: &OwnedFd, allowed: &HashSet<PathBuf>, data: &[u8]) {
    let path = Path::new(OsStr::from_bytes(data));

    if !allowed.contains(path) {
        if let Err(e) = socket.send(&ipc::build_response(libc::EACCES as u8, 0)) {
            warn!("[SERVER] failed to send EACCES response: {e}");
        }
        return;
    }

    // The underlying filesystem is static, so this is safe
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(f) => f,
        Err(e) => {
            let errno = e.raw_os_error().unwrap_or(libc::EIO) as u8;
            if let Err(e) = socket.send(&ipc::build_response(errno, 0)) {
                warn!("[SERVER] failed to send open error response: {e}");
            }
            return;
        }
    };

    let id = match BackingId::create_raw(fuse_fd.as_fd(), &file) {
        Ok(id) => id,
        Err(e) => {
            error!("[SERVER] BackingId::create_raw failed: {e}");
            if let Err(e) = socket.send(&ipc::build_response(libc::EIO as u8, 0)) {
                warn!("[SERVER] failed to send EIO response: {e}");
            }
            return;
        }
    };

    // Kernel now holds its own reference to the file; the local fd is no longer needed
    drop(file);

    let resp = ipc::build_response(0, id);
    if let Err(e) = socket.send(&resp) {
        error!("[SERVER] failed to send open response (child gone): {e}");
    }
}

fn handle_close(fuse_fd: &OwnedFd, data: &[u8]) {
    let Some(id) = ipc::parse_close_request(data) else {
        warn!(
            "[SERVER] short close message (expected 4 bytes, got {})",
            data.len()
        );
        return;
    };
    if let Err(e) = unsafe { backing_close(fuse_fd.as_raw_fd(), &id) } {
        error!("[SERVER] error closing backing id {id}: {e}");
    }
}
