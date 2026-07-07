use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use std::time::{Duration, SystemTime};

use log::{error, warn};

use parking_lot::Mutex;

use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, INodeNo,
    InitFlags, KernelConfig, LockOwner, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite,
    Request, TimeOrNow, WriteFlags,
};
use slotmap::{DefaultKey, Key, KeyData, SlotMap};

use crate::filter::{self, ViewFile};
use crate::ipc;

// TTL For Kernel Caching. Can be fairly large due to
// the underlying filesystem being static.
const TTL: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, Debug, PartialEq, Hash, Eq, PartialOrd, Ord)]
pub struct NodeID(usize);

impl NodeID {
    pub fn new(f: usize) -> NodeID {
        NodeID(f)
    }
}

impl From<usize> for NodeID {
    fn from(value: usize) -> Self {
        NodeID(value)
    }
}

impl From<NodeID> for INodeNo {
    fn from(value: NodeID) -> Self {
        INodeNo(value.0 as u64 + 1)
    }
}

impl From<NodeID> for usize {
    fn from(value: NodeID) -> Self {
        value.0
    }
}

#[derive(Debug)]
pub struct FileNode {
    name: OsString,
    parent: NodeID,
    children: Vec<NodeID>,
    is_dir: bool,
    path: PathBuf,
}

struct HandleCache {
    by_handle: SlotMap<DefaultKey, u32>,
    by_nodeid: HashMap<NodeID, u32>,
}

impl HandleCache {
    pub fn new() -> HandleCache {
        HandleCache {
            by_handle: SlotMap::new(),
            by_nodeid: HashMap::new(),
        }
    }

    pub fn new_handle(&mut self, raw_id: u32) -> u64 {
        let fh = self.by_handle.insert(raw_id);
        fh.data().as_ffi()
    }
}

pub struct ViewFS {
    nodes: Vec<FileNode>,
    handle_cache: Mutex<HandleCache>,
    socket: UnixDatagram,
    root_attr: FileAttr,
}

impl ViewFS {
    pub fn new(socket: UnixDatagram, paths: Vec<ViewFile>, mount_path: PathBuf) -> ViewFS {
        // Stat the mount path before mount
        let metadata = mount_path.metadata().expect("Mount path must exist");
        let mut perms = metadata.permissions();
        perms.set_readonly(true);

        let root_attr = FileAttr {
            ino: NodeID(0).into(),
            size: metadata.size(),
            blocks: metadata.blocks(),
            atime: metadata.accessed().unwrap_or(UNIX_EPOCH),
            mtime: metadata.modified().unwrap_or(UNIX_EPOCH),
            ctime: UNIX_EPOCH
                + Duration::new(
                    metadata.ctime().max(0) as u64,
                    metadata.ctime_nsec().max(0) as u32,
                ),
            crtime: metadata.created().unwrap_or(UNIX_EPOCH),
            kind: FileType::Directory,
            perm: perms.mode() as u16,
            uid: metadata.uid(),
            gid: metadata.gid(),
            rdev: metadata.rdev() as u32,
            blksize: metadata.blksize() as u32,
            flags: 0,
            nlink: 2,
        };

        let mut viewfs = ViewFS {
            nodes: Vec::new(),
            handle_cache: Mutex::new(HandleCache::new()),
            socket,
            root_attr,
        };
        viewfs.generate_nodes(paths);
        viewfs
    }

    fn generate_nodes(&mut self, mut paths: Vec<ViewFile>) {
        let mut path_map = HashMap::new();

        // Sort paths by filepath
        paths.sort_by(|a, b| a.path.cmp(&b.path));

        // Add root to nodes
        let root_path = PathBuf::from("/");
        path_map.insert(root_path.clone(), NodeID(0));

        self.nodes.push(FileNode {
            name: "/".into(),
            path: "/".into(),
            parent: NodeID(0),
            children: Vec::new(),
            is_dir: true,
        });

        'path_loop: for file in paths {
            let mut ancestors = file.path.ancestors();
            let mut ancestor_stack = Vec::new();
            let Some(mut current_ancestor) = ancestors.next() else {
                error!("File has no ancestors");
                continue;
            };

            while !path_map.contains_key(current_ancestor) {
                ancestor_stack.push(current_ancestor);
                let Some(next_ancestor) = ancestors.next() else {
                    error!(
                        "Relative Paths are Unsupported: {}",
                        file.path.to_string_lossy()
                    );
                    continue 'path_loop;
                };
                current_ancestor = next_ancestor;
            }

            let mut parent_id = *path_map
                .get(current_ancestor)
                .expect("Ancestor should be in pathmap");

            let mut it = ancestor_stack.into_iter().rev().peekable();
            while let Some(path) = it.next() {
                let new_id: NodeID = self.nodes.len().into();

                path_map.insert(path.to_path_buf(), new_id);

                self.nodes
                    .get_mut(usize::from(parent_id))
                    .expect("Parent must exist")
                    .children
                    .push(new_id);

                self.nodes.push(FileNode {
                    name: path
                        .file_name()
                        .expect("File node should not be root or ..")
                        .to_os_string(),
                    path: path.into(),
                    parent: parent_id,
                    children: Vec::new(),
                    is_dir: it.peek().is_some() || file.filetype == filter::FileType::Dir,
                });

                parent_id = new_id;
            }
        }
    }

    #[inline]
    fn get_node(&self, id: NodeID) -> Option<&FileNode> {
        self.nodes.get(usize::from(id))
    }

    #[inline]
    fn node_filetype(&self, node: &FileNode) -> FileType {
        match node.is_dir {
            true => FileType::Directory,
            false => FileType::RegularFile,
        }
    }

    #[inline]
    fn get_inode(&self, inode: &INodeNo) -> Option<NodeID> {
        if inode.0 == 0 {
            return None;
        }

        let id = NodeID(inode.0 as usize - 1);

        if usize::from(id) < self.nodes.len() {
            Some(id)
        } else {
            None
        }
    }

    #[inline]
    fn get_node_inode(&self, inode: &INodeNo) -> Option<(NodeID, &FileNode)> {
        let id = self.get_inode(inode)?;
        let node = self.get_node(id)?;

        Some((id, node))
    }

    fn get_file_attr(&self, id: NodeID) -> Option<FileAttr> {
        let node = self.get_node(id)?;

        if id == NodeID(0) {
            let mut attr = self.root_attr;

            attr.nlink = 2 + node
                .children
                .iter()
                .filter(|&&id| self.get_node(id).map(|x| x.is_dir).unwrap_or(false))
                .count() as u32;

            return Some(attr);
        }

        let metadata = node.path.metadata().ok()?;

        // Set file as readonly
        let mut perms = metadata.permissions();
        perms.set_readonly(true);
        let perm_mode = perms.mode() as u16;

        let filetype = self.node_filetype(node);

        let nlink: u32 = if node.is_dir {
            2 + node
                .children
                .iter()
                .filter(|&&id| self.get_node(id).map(|x| x.is_dir).unwrap_or(false))
                .count() as u32
        } else {
            1
        };

        Some(FileAttr {
            ino: id.into(),
            size: metadata.size(),
            blocks: metadata.blocks(),
            atime: metadata.accessed().unwrap_or(UNIX_EPOCH),
            mtime: metadata.modified().unwrap_or(UNIX_EPOCH),
            ctime: UNIX_EPOCH
                + Duration::new(
                    metadata.ctime().max(0) as u64,
                    metadata.ctime_nsec().max(0) as u32,
                ),
            crtime: metadata.created().unwrap_or(UNIX_EPOCH),
            kind: filetype,
            perm: perm_mode,
            uid: metadata.uid(),
            gid: metadata.gid(),
            rdev: metadata.rdev() as u32,
            blksize: metadata.blksize() as u32,
            flags: 0, // Mac only flags
            nlink,
        })
    }
}

impl Filesystem for ViewFS {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        let Ok(_) = config.add_capabilities(InitFlags::FUSE_PASSTHROUGH) else {
            error!("Kernel does not support FUSE_PASSTHROUGH; update to 6.9+");
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no FUSE_PASSTHROUGH",
            ));
        };

        config.set_max_stack_depth(2).ok();
        Ok(())
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let bs: u64 = 4096;
        let files = self.nodes.len() as u64;
        reply.statfs(0, 0, 0, files, 0, bs as u32, 255, bs as u32);
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some((_, parent_node)) = self.get_node_inode(&parent) else {
            error!("Invalid NodeId passed");
            reply.error(Errno::ENOENT);
            return;
        };

        for node_id in &parent_node.children {
            let node = self.get_node(*node_id).expect("Children should be valid");

            if node.name == name {
                let Some(node_attr) = self.get_file_attr(*node_id) else {
                    error!("Unable to get attr from underlying filesystem");
                    reply.error(Errno::EIO);
                    return;
                };

                reply.entry(&TTL, &node_attr, fuser::Generation(0));
                return;
            }
        }

        reply.error(Errno::ENOENT);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(node_id) = self.get_inode(&ino) else {
            error!("Invalid NodeId passed");
            reply.error(Errno::ENOENT);
            return;
        };

        let Some(node_attr) = &self.get_file_attr(node_id) else {
            error!("Unable to get attr from underlying filesystem");
            reply.error(Errno::EIO);
            return;
        };

        reply.attr(&TTL, node_attr);
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            reply.error(Errno::EROFS);
            return;
        }

        let Some((node_id, node)) = self.get_node_inode(&ino) else {
            reply.error(Errno::ENOENT);
            return;
        };

        // Check cache for backing ID
        let raw_id = {
            let cache = self.handle_cache.lock();
            cache.by_nodeid.get(&node_id).copied()
        };

        if let Some(raw_id) = raw_id {
            let mut cache = self.handle_cache.lock();
            let fh = cache.new_handle(raw_id);
            drop(cache);
            let bid = unsafe { reply.wrap_backing(raw_id) };
            reply.opened_passthrough(FileHandle(fh), FopenFlags::empty(), &bid);
            bid.into_raw();
            return;
        }

        // Request new backing ID from the privileged parent
        let msg = ipc::build_open_request(&node.path);

        if self.socket.send(&msg).is_err() {
            error!("Failed to send open request to parent");
            reply.error(Errno::EIO);
            return;
        }

        let mut buf = [0u8; 5];
        if self.socket.recv(&mut buf).map_or(true, |n| n != 5) {
            reply.error(Errno::EIO);
            return;
        }

        let new_id = match ipc::parse_open_response(buf) {
            Ok(id) => id,
            Err(errno) => {
                reply.error(Errno::from_i32(errno as i32));
                return;
            }
        };

        // Double-check insert under lock (another thread may have beaten us)
        let mut cache = self.handle_cache.lock();
        let raw_id = match cache.by_nodeid.entry(node_id) {
            Entry::Occupied(e) => {
                let msg = ipc::build_close_request(new_id);
                if let Err(e) = self.socket.send(&msg) {
                    warn!("failed to send close for duplicate open: {e}");
                }
                *e.get()
            }
            Entry::Vacant(e) => *e.insert(new_id),
        };

        let fh = cache.new_handle(raw_id);
        drop(cache);

        let bid = unsafe { reply.wrap_backing(raw_id) };
        reply.opened_passthrough(FileHandle(fh), FopenFlags::empty(), &bid);
        bid.into_raw();
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let Some(node_id) = self.get_inode(&ino) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let mut cache = self.handle_cache.lock();
        if let Some(raw_id) = cache.by_handle.remove(KeyData::from_ffi(fh.0).into()) {
            // If no other open handles reference this backing_id, close it
            if !cache.by_handle.values().any(|&id| id == raw_id) {
                cache.by_nodeid.remove(&node_id);
                let msg = ipc::build_close_request(raw_id);
                if let Err(e) = self.socket.send(&msg) {
                    warn!("failed to send close: {e}");
                }
            }
        }

        reply.ok();
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some((dir_id, dir_node)) = self.get_node_inode(&ino) else {
            error!("Invalid NodeId passed");
            reply.error(Errno::ENOENT);
            return;
        };

        let mut entries = vec![
            (dir_id, FileType::Directory, OsStr::new(".")),
            (dir_node.parent, FileType::Directory, OsStr::new("..")),
        ];

        for node_id in &dir_node.children {
            let child_node = self.get_node(*node_id).expect("Children should be valid");
            entries.push((
                *node_id,
                self.node_filetype(child_node),
                child_node.name.as_os_str(),
            ));
        }

        for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
            // i + 1 means the index of the next entry
            if reply.add(INodeNo::from(entry.0), (i + 1) as u64, entry.1, entry.2) {
                break;
            }
        }

        reply.ok();
    }

    // Read only function calls
    fn setattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        reply.error(Errno::EROFS);
    }

    fn create(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        reply.error(Errno::EROFS);
    }

    fn mkdir(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn unlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn symlink(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _link_name: &OsStr,
        _target: &Path,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn rename(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _newparent: INodeNo,
        _newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EROFS);
    }

    fn link(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _offset: u64,
        _data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        reply.error(Errno::EROFS);
    }
}
