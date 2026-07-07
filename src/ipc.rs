use std::{
    collections::HashSet,
    ffi::OsStr,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            ffi::OsStrExt,
            net::UnixDatagram,
        },
    },
    path::{Path, PathBuf},
};

use nix::sys::socket::{self, ControlMessage, ControlMessageOwned, MsgFlags};

#[repr(u8)]
pub enum Tag {
    Open = 0,
    Close = 1,
}

impl TryFrom<u8> for Tag {
    type Error = u8;

    fn try_from(v: u8) -> Result<Self, u8> {
        match v {
            0 => Ok(Self::Open),
            1 => Ok(Self::Close),
            _ => Err(v),
        }
    }
}

pub fn serialize_paths(paths: &[impl AsRef<Path>]) -> Vec<u8> {
    let mut buf = Vec::new();
    let count = paths.len() as u32;
    buf.extend_from_slice(&count.to_le_bytes());
    for path in paths {
        let bytes = path.as_ref().as_os_str().as_encoded_bytes();
        let len = bytes.len() as u32;
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(bytes);
    }
    buf
}

pub fn deserialize_paths(data: &[u8]) -> Option<Vec<PathBuf>> {
    if data.len() < 4 {
        return None;
    }
    let count = u32::from_le_bytes(data[..4].try_into().ok()?) as usize;
    let mut paths = Vec::with_capacity(count);
    let mut offset = 4;
    for _ in 0..count {
        if offset + 4 > data.len() {
            return None;
        }
        let len = u32::from_le_bytes(data[offset..offset + 4].try_into().ok()?) as usize;
        offset += 4;
        if offset + len > data.len() {
            return None;
        }
        let path = PathBuf::from(OsStr::from_bytes(&data[offset..offset + len]));
        paths.push(path);
        offset += len;
    }
    Some(paths)
}

pub fn build_open_request(path: &Path) -> Vec<u8> {
    let bytes = path.as_os_str().as_encoded_bytes();
    let mut msg = Vec::with_capacity(1 + bytes.len());
    msg.push(Tag::Open as u8);
    msg.extend_from_slice(bytes);
    msg
}

pub fn build_close_request(backing_id: u32) -> [u8; 5] {
    let mut msg = [0u8; 5];
    msg[0] = Tag::Close as u8;
    msg[1..5].copy_from_slice(&backing_id.to_le_bytes());
    msg
}

pub fn build_response(errno: u8, backing_id: u32) -> [u8; 5] {
    let mut resp = [0u8; 5];
    resp[0] = errno;
    resp[1..5].copy_from_slice(&backing_id.to_le_bytes());
    resp
}

pub fn parse_open_response(buf: [u8; 5]) -> Result<u32, u8> {
    if buf[0] != 0 {
        return Err(buf[0]);
    }
    Ok(u32::from_le_bytes(buf[1..5].try_into().expect("slice is exactly 4 bytes")))
}

pub fn parse_close_request(data: &[u8]) -> Option<u32> {
    if data.len() != 4 {
        return None;
    }
    Some(u32::from_le_bytes(data.try_into().ok()?))
}

pub fn send_whitelist(sock: &UnixDatagram, paths: &[impl AsRef<Path>]) -> io::Result<()> {
    let data = serialize_paths(paths);
    sock.send(&(data.len() as u32).to_le_bytes())?;
    sock.send(&data)?;
    Ok(())
}

pub fn recv_whitelist(sock: &UnixDatagram) -> io::Result<HashSet<PathBuf>> {
    let mut size_buf = [0u8; 4];
    if sock.recv(&mut size_buf)? != 4 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short whitelist length"));
    }
    let size = u32::from_le_bytes(size_buf) as usize;
    let mut data = vec![0u8; size];
    if sock.recv(&mut data)? != size {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "whitelist data size mismatch"));
    }
    deserialize_paths(&data)
        .map(|paths| paths.into_iter().collect())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed whitelist"))
}

pub fn send_fuse_fd(sock: &UnixDatagram, fd: RawFd) -> io::Result<()> {
    let fds = [fd];
    socket::sendmsg::<()>(
        sock.as_raw_fd(),
        &[],
        &[ControlMessage::ScmRights(&fds)],
        MsgFlags::empty(),
        None,
    )?;
    Ok(())
}

pub fn recv_fuse_fd(sock: &UnixDatagram) -> io::Result<OwnedFd> {
    let mut cmsg_buf = [0u8; 64];
    let msg = socket::recvmsg::<()>(
        sock.as_raw_fd(),
        &mut [],
        Some(&mut cmsg_buf[..]),
        MsgFlags::empty(),
    )?;

    let cmsgs = msg.cmsgs().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to parse control messages: {e}"),
        )
    })?;

    for cmsg in cmsgs {
        if let ControlMessageOwned::ScmRights(fds) = cmsg {
            if let Some(&fd) = fds.first() {
                let owned = unsafe { OwnedFd::from_raw_fd(fd) };
                return Ok(owned);
            }
            return Err(io::Error::new(io::ErrorKind::InvalidData, "empty SCM_RIGHTS"));
        }
    }

    Err(io::Error::new(io::ErrorKind::InvalidData, "no SCM_RIGHTS in message"))
}
