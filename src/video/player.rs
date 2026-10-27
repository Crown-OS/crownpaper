//! Playback: frames from the helper, paced against the wall clock.
//!
//! Frames are only pulled from the socket on the outputs' frame callbacks,
//! and the helper stops decoding once enough of them are unreleased — so a
//! wallpaper the compositor stops asking frames for costs nothing.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crownpaper::shm::ShmImage;
use crownpaper::util::cover::Size;
use crownpaper::video_wire::{FrameLayout, FromHelper, PlaneLayout, ToHelper, recv, send};
use rustix::event::{PollFd, PollFlags, Timespec, poll};

use super::VideoHelper;

/// How far the wall clock may drift from the stream before playback
/// re-anchors instead of fast-forwarding.
const RESYNC_SECONDS: f64 = 0.5;
const SNAPSHOT_TIMEOUT: Timespec = Timespec {
    tv_sec: 1,
    tv_nsec: 0,
};

pub struct DmabufPlanes {
    pub size: Size,
    pub fourcc: u32,
    pub planes: Vec<(OwnedFd, PlaneLayout)>,
}

pub enum FrameContent {
    Dmabuf(DmabufPlanes),
    Shm(ShmImage),
}

/// One decoded frame. Dropping it tells the helper the buffer is free.
pub struct VideoFrame {
    id: u64,
    time: f64,
    pub content: FrameContent,
    socket: Arc<OwnedFd>,
}

impl VideoFrame {
    pub fn size(&self) -> Size {
        match &self.content {
            FrameContent::Dmabuf(dmabuf) => dmabuf.size,
            FrameContent::Shm(image) => image.size(),
        }
    }
}

impl Drop for VideoFrame {
    fn drop(&mut self) {
        // The helper may already be gone; then there is nothing to release.
        let _ = send(
            self.socket.as_fd(),
            &ToHelper::Release { frame: self.id },
            &[],
        );
    }
}

pub struct Player {
    queue: VecDeque<VideoFrame>,
    origin: Option<Instant>,
    current: Option<Arc<VideoFrame>>,
    helper: VideoHelper,
}

impl Player {
    pub fn new(helper: VideoHelper) -> Self {
        Self {
            queue: VecDeque::new(),
            origin: None,
            current: None,
            helper,
        }
    }

    pub fn current(&self) -> Option<&Arc<VideoFrame>> {
        self.current.as_ref()
    }

    /// When the next queued frame is due, if one is queued.
    pub fn next_due(&self, now: Instant) -> Option<Instant> {
        let head = self.queue.front()?;
        Some(
            self.origin
                .map_or(now, |origin| origin + Duration::from_secs_f64(head.time)),
        )
    }

    /// The newest frame whose time has come, if it differs from the one
    /// already on screen.
    pub fn take_due(&mut self, now: Instant) -> Option<Arc<VideoFrame>> {
        if let Err(e) = self.receive() {
            log::warn!("video helper: {e:#}");
        }
        let head_time = self.queue.front()?.time;
        let origin = *self
            .origin
            .get_or_insert_with(|| now - Duration::from_secs_f64(head_time));
        let clock = now.saturating_duration_since(origin).as_secs_f64();
        let mut due = None;
        while self.queue.front().is_some_and(|frame| frame.time <= clock) {
            due = self.queue.pop_front();
        }
        let due = Arc::new(due?);
        if clock - due.time > RESYNC_SECONDS {
            self.origin = Some(now - Duration::from_secs_f64(due.time));
        }
        self.current = Some(Arc::clone(&due));
        Some(due)
    }

    /// The frame on screen as a screen-sized still, for transitioning away.
    pub fn snapshot(&mut self) -> Result<Option<ShmImage>> {
        let Some(current) = self.current.clone() else {
            return Ok(None);
        };
        if let FrameContent::Shm(image) = &current.content {
            let mut still = ShmImage::new(image.size())?;
            still.pixels_mut().copy_from_slice(image.pixels());
            return Ok(Some(still));
        }
        let socket = Arc::clone(&self.helper.socket);
        send(
            socket.as_fd(),
            &ToHelper::Snapshot { frame: current.id },
            &[],
        )?;
        loop {
            let mut fds = [PollFd::new(&*socket, PollFlags::IN)];
            if poll(&mut fds, Some(&SNAPSHOT_TIMEOUT))? == 0 {
                bail!("the video helper did not answer the snapshot request");
            }
            match self.next_message()? {
                Some(Message::Snapshot(still)) => return Ok(Some(still)),
                Some(Message::Frame(frame)) => self.queue.push_back(frame),
                None => bail!("the video helper exited"),
            }
        }
    }

    fn receive(&mut self) -> Result<()> {
        loop {
            match self.next_message() {
                Ok(Some(Message::Frame(frame))) => self.queue.push_back(frame),
                Ok(Some(Message::Snapshot(_))) => {}
                Ok(None) => bail!("the video helper exited"),
                Err(e) if is_would_block(&e) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    fn next_message(&mut self) -> Result<Option<Message>> {
        let socket = Arc::clone(&self.helper.socket);
        let Some((message, mut fds)) = recv::<FromHelper>(socket.as_fd())? else {
            return Ok(None);
        };
        Ok(Some(match message {
            FromHelper::Frame { id, time, layout } => {
                let content = match layout {
                    FrameLayout::Shm { size } => FrameContent::Shm(ShmImage::from_fd(
                        fds.pop().context("frame without pixels")?,
                        size,
                    )?),
                    FrameLayout::Dmabuf {
                        size,
                        fourcc,
                        planes,
                    } => {
                        if fds.len() != planes.len() {
                            bail!(
                                "frame carries {} fds for {} planes",
                                fds.len(),
                                planes.len()
                            );
                        }
                        FrameContent::Dmabuf(DmabufPlanes {
                            size,
                            fourcc,
                            planes: fds.into_iter().zip(planes).collect(),
                        })
                    }
                };
                Message::Frame(VideoFrame {
                    id,
                    time,
                    content,
                    socket,
                })
            }
            FromHelper::Snapshot { size, .. } => Message::Snapshot(ShmImage::from_fd(
                fds.pop().context("snapshot without pixels")?,
                size,
            )?),
            FromHelper::Poster { .. } => bail!("unexpected poster"),
            FromHelper::Failed(reason) => bail!(reason),
        }))
    }
}

enum Message {
    Frame(VideoFrame),
    Snapshot(ShmImage),
}

fn is_would_block(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|e| e.kind() == io::ErrorKind::WouldBlock)
}
