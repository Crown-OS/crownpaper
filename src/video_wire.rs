//! The daemon ↔ `crownpaper-video` protocol.
//!
//! One `SOCK_SEQPACKET` socketpair per video: every message is a single
//! postcard-encoded packet, and pixel buffers travel alongside as file
//! descriptors (dmabufs or memfds), so frames are never copied through the
//! socket.

use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{BorrowedFd, OwnedFd};

use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, SocketFlags, SocketType, recvmsg, sendmsg,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::util::cover::Size;

/// Frames the helper may have in flight before the daemon releases one:
/// the daemon's queue plus what the compositor still holds.
pub const MAX_OUTSTANDING_FRAMES: usize = 6;
const MAX_FDS: usize = 4;
const MAX_PACKET: usize = 64 * 1024;

#[derive(Serialize, Deserialize)]
pub enum ToHelper {
    Start {
        path: String,
        screen: Size,
        render_node: Option<String>,
        /// `(fourcc, modifier)` pairs the compositor can import.
        importable: Vec<(u32, u64)>,
    },
    /// The daemon is done with a frame; its buffer may be reused.
    Release { frame: u64 },
    /// Send frame `frame` back as a screen-sized still.
    Snapshot { frame: u64 },
}

#[derive(Serialize, Deserialize)]
pub enum FromHelper {
    /// The first frame as a still; one memfd.
    Poster {
        size: Size,
    },
    /// One fd per dmabuf plane, or one memfd.
    Frame {
        id: u64,
        time: f64,
        layout: FrameLayout,
    },
    /// One memfd.
    Snapshot {
        frame: u64,
        size: Size,
    },
    Failed(String),
}

#[derive(Serialize, Deserialize)]
pub enum FrameLayout {
    Dmabuf {
        size: Size,
        fourcc: u32,
        planes: Vec<PlaneLayout>,
    },
    Shm {
        size: Size,
    },
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct PlaneLayout {
    pub offset: u32,
    pub stride: u32,
    pub modifier: u64,
}

pub fn socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
    Ok(rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?)
}

pub fn send<T: Serialize>(
    socket: BorrowedFd<'_>,
    message: &T,
    fds: &[BorrowedFd<'_>],
) -> io::Result<()> {
    let bytes = postcard::to_stdvec(message).map_err(io::Error::other)?;
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !fds.is_empty() && !control.push(SendAncillaryMessage::ScmRights(fds)) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "too many fds"));
    }
    sendmsg(
        socket,
        &[IoSlice::new(&bytes)],
        &mut control,
        SendFlags::NOSIGNAL,
    )?;
    Ok(())
}

/// The next message, or `None` once the peer has hung up. A non-blocking
/// socket with nothing queued yields `ErrorKind::WouldBlock`.
pub fn recv<T: DeserializeOwned>(socket: BorrowedFd<'_>) -> io::Result<Option<(T, Vec<OwnedFd>)>> {
    let mut bytes = [0u8; MAX_PACKET];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let received = recvmsg(
        socket,
        &mut [IoSliceMut::new(&mut bytes)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
    )?;
    let fds: Vec<OwnedFd> = control
        .drain()
        .filter_map(|message| match message {
            RecvAncillaryMessage::ScmRights(fds) => Some(fds),
            _ => None,
        })
        .flatten()
        .collect();
    if received.bytes == 0 {
        return Ok(None);
    }
    let message = postcard::from_bytes(&bytes[..received.bytes]).map_err(io::Error::other)?;
    Ok(Some((message, fds)))
}
