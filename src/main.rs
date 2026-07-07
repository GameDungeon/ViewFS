use std::sync::Arc;
use std::thread;

use parking_lot::Mutex;

use anyhow::Result;
use clap::Parser;
use fuser::{Config, SessionACL};
use nix::{
    sys::signal::{SigSet, Signal},
    unistd::{Gid, Uid},
};

use crate::filesystem::ViewFS;
use std::{
    os::fd::{AsFd, AsRawFd},
    os::unix::net::UnixDatagram,
    path::PathBuf,
};

pub mod backing_id;
pub mod filesystem;
pub mod filter;
pub mod ipc;
pub mod parse;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// File containing filter data
    file: PathBuf,

    /// Mount Point
    mount: PathBuf,
}

fn downgrade_process() -> Result<()> {
    // If run as sudo downgrade process back to user
    if let Ok(uid) = std::env::var("SUDO_UID") &&
        let Ok(gid) = std::env::var("SUDO_GID") {
        nix::unistd::setgid(Gid::from_raw(gid.parse()?))?;
        nix::unistd::setuid(Uid::from_raw(uid.parse()?))?;
    }

    // Clear all capabilities
    caps::clear(None, caps::CapSet::Effective)?;
    caps::clear(None, caps::CapSet::Permitted)?;

    Ok(())
}

fn unmount_thread(session: &mut fuser::Session<ViewFS>) -> Result<()> {
    let unmounter = Arc::new(Mutex::new(session.unmount_callable()));
    let u = Arc::clone(&unmounter);

    // Block SIGINT/SIGTERM in this thread
    let mut sigset = SigSet::empty();
    sigset.add(Signal::SIGINT);
    sigset.add(Signal::SIGTERM);
    sigset.thread_block()?;

    // Spawn a signal thread that calls unmount() on SIGINT/SIGTERM
    thread::spawn(move || {
        if let Ok(sig) = sigset.wait()
            && matches!(sig, Signal::SIGINT | Signal::SIGTERM)
        {
            u.lock().unmount().ok();
        }
    });

    Ok(())
}

fn main() -> Result<()> {
    // This needs `CAP_SYS_ADMIN` to support opening backing ids for fuse passthrough
    // So we fork into a privileged parent that handles the backing ids and an unprivileged child
    // that handles the rest
    let (parent_sock, child_sock) = UnixDatagram::pair()?;

    // Fork now as forking isn't guaranteed to be a safe operation once e.g. libraries get involved
    match unsafe { nix::unistd::fork()? } {
        nix::unistd::ForkResult::Parent { child } => {
            drop(child_sock);
            env_logger::init();
            backing_id::backing_id_server(parent_sock, child);
            return Ok(());
        }
        nix::unistd::ForkResult::Child => {
            drop(parent_sock);
        }
    }

    // Downgrade child
    downgrade_process()?;

    env_logger::init();

    let args = Args::parse();

    // Parse filters
    let filters = parse::parse_filter(args.file)?;
    let all_files = filter::generate_paths(filters)?;

    // Send whitelist to parent
    let whitelist_paths: Vec<&PathBuf> = all_files.iter().map(|vf| &vf.path).collect();
    ipc::send_whitelist(&child_sock, &whitelist_paths)?;

    // Mount Filesystem
    let viewfs = ViewFS::new(child_sock.try_clone()?, all_files, args.mount.clone());

    let mut mount_config = Config::default();
    mount_config.acl = SessionACL::All;

    let mut session = fuser::Session::new(viewfs, &args.mount, &mount_config)?;

    // Start thread to unmount on SIGINT/SIGTERM
    unmount_thread(&mut session)?;

    // Send the FUSE FD to the parent process so it may open backing files
    ipc::send_fuse_fd(&child_sock, session.as_fd().as_raw_fd())?;

    session.run()?;

    Ok(())
}
