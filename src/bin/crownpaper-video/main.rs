//! crownpaper-video — plays one video wallpaper for the crownpaper daemon.
//!
//! The daemon spawns this helper per video and talks to it over the
//! socketpair whose fd is the only argument (see `crownpaper::video_wire`).
//! FFmpeg and its dependency tree therefore only occupy memory while a video
//! is actually shown, and are gone the moment the helper exits.
//!
//! Frames are decoded on the compositor's GPU through VA-API and handed over
//! as dmabufs; the helper keeps each one alive until the daemon releases it,
//! and stops decoding while [`MAX_OUTSTANDING_FRAMES`] are unreleased — so a
//! covered wallpaper, whose frames are never consumed, costs nothing.

mod convert;
mod dmabuf;
mod hw;
mod source;

use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use crownpaper::shm::ShmImage;
use crownpaper::util::cover::{Size, cover_size};
use crownpaper::video_wire::{
    FrameLayout, FromHelper, MAX_OUTSTANDING_FRAMES, PlaneLayout, ToHelper, recv, send,
};
use ffmpeg_next::frame;
use rustix::event::{PollFd, PollFlags, Timespec, poll};

use convert::Converter;
use dmabuf::DmabufFrame;
use source::Source;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();
    let Some(raw_socket) = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse::<RawFd>().ok())
    else {
        eprintln!("crownpaper-video is started by `crownpaper daemon`");
        return ExitCode::FAILURE;
    };
    // SAFETY: the daemon hands this process exactly one inherited socket fd,
    // which nothing else here owns.
    let socket = unsafe { OwnedFd::from_raw_fd(raw_socket) };
    match serve(socket.as_fd()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::warn!("{e:#}");
            let _ = send(socket.as_fd(), &FromHelper::Failed(format!("{e:#}")), &[]);
            ExitCode::FAILURE
        }
    }
}

fn serve(socket: BorrowedFd<'_>) -> Result<()> {
    let Some((
        ToHelper::Start {
            path,
            screen,
            render_node,
            importable,
        },
        _,
    )) = recv(socket)?
    else {
        bail!("expected a start request");
    };
    if let Err(e) = ffmpeg_next::init() {
        log::warn!("ffmpeg init: {e}");
    }
    ffmpeg_next::util::log::set_level(ffmpeg_next::util::log::Level::Error);

    let mut source = Source::open(Path::new(&path), render_node.as_deref().map(Path::new))
        .with_context(|| format!("opening {path}"))?;
    let (first, time) = source.next_frame()?;
    let mut session = Session {
        socket,
        screen,
        importable,
        converter: Converter::default(),
        held: HashMap::with_capacity(MAX_OUTSTANDING_FRAMES),
        free_images: Vec::new(),
        next_id: 0,
        warned_fallback: false,
    };
    let poster = still_of(&mut session.converter, &first, screen)?;
    send(
        socket,
        &FromHelper::Poster {
            size: poster.size(),
        },
        &[poster.as_fd()],
    )?;
    drop(poster);

    session.publish(first, time)?;
    while session.serve_requests()? {
        let (frame, time) = source.next_frame()?;
        session.publish(frame, time)?;
    }
    Ok(())
}

struct Session<'a> {
    socket: BorrowedFd<'a>,
    screen: Size,
    importable: Vec<(u32, u64)>,
    converter: Converter,
    held: HashMap<u64, Held>,
    free_images: Vec<ShmImage>,
    next_id: u64,
    warned_fallback: bool,
}

/// A frame the daemon may still be showing.
enum Held {
    Dmabuf {
        mapped: DmabufFrame,
        decoded: frame::Video,
    },
    Shm(ShmImage),
}

impl Session<'_> {
    fn publish(&mut self, decoded: frame::Video, time: f64) -> Result<()> {
        let id = self.next_id;
        self.next_id += 1;
        let held = match self.export(&decoded) {
            Some(mapped) => Held::Dmabuf { mapped, decoded },
            None => {
                if !std::mem::replace(&mut self.warned_fallback, true) {
                    log::info!("video frames go through shared memory");
                }
                Held::Shm(self.shm_frame(&decoded)?)
            }
        };
        let (layout, fds): (FrameLayout, Vec<BorrowedFd<'_>>) = match &held {
            Held::Dmabuf { mapped, .. } => (
                FrameLayout::Dmabuf {
                    size: mapped.size,
                    fourcc: mapped.fourcc,
                    planes: mapped
                        .planes
                        .iter()
                        .map(|plane| PlaneLayout {
                            offset: plane.offset,
                            stride: plane.stride,
                            modifier: plane.modifier,
                        })
                        .collect(),
                },
                mapped.planes.iter().map(dmabuf::Plane::fd).collect(),
            ),
            Held::Shm(image) => (FrameLayout::Shm { size: image.size() }, vec![image.as_fd()]),
        };
        send(self.socket, &FromHelper::Frame { id, time, layout }, &fds)?;
        self.held.insert(id, held);
        Ok(())
    }

    /// Handles daemon requests; blocks while too many frames are in flight.
    /// Returns false once the daemon has gone away.
    fn serve_requests(&mut self) -> Result<bool> {
        loop {
            let blocking = self.held.len() >= MAX_OUTSTANDING_FRAMES;
            let mut fds = [PollFd::new(&self.socket, PollFlags::IN)];
            let timeout = (!blocking).then_some(Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            });
            if poll(&mut fds, timeout.as_ref())? == 0 {
                return Ok(true);
            }
            match recv::<ToHelper>(self.socket)? {
                None => return Ok(false),
                Some((ToHelper::Release { frame }, _)) => self.release(frame),
                Some((ToHelper::Snapshot { frame }, _)) => self.snapshot(frame)?,
                Some((ToHelper::Start { .. }, _)) => {}
            }
        }
    }

    fn release(&mut self, frame: u64) {
        if let Some(Held::Shm(image)) = self.held.remove(&frame)
            && self.free_images.len() < MAX_OUTSTANDING_FRAMES
        {
            self.free_images.push(image);
        }
    }

    fn snapshot(&mut self, frame: u64) -> Result<()> {
        let still = match self.held.get(&frame) {
            Some(Held::Dmabuf { decoded, .. }) => {
                still_of(&mut self.converter, decoded, self.screen)?
            }
            Some(Held::Shm(image)) => {
                let mut still = ShmImage::new(image.size())?;
                still.pixels_mut().copy_from_slice(image.pixels());
                still
            }
            None => bail!("snapshot of unknown frame {frame}"),
        };
        send(
            self.socket,
            &FromHelper::Snapshot {
                frame,
                size: still.size(),
            },
            &[still.as_fd()],
        )?;
        Ok(())
    }

    fn shm_frame(&mut self, decoded: &frame::Video) -> Result<ShmImage> {
        let size = cover_size(Source::frame_size(decoded), self.screen);
        let mut image = match self
            .free_images
            .iter()
            .position(|image| image.size() == size)
        {
            Some(index) => self.free_images.swap_remove(index),
            None => ShmImage::new(size)?,
        };
        self.converter.convert(decoded, &mut image)?;
        Ok(image)
    }

    fn export(&self, decoded: &frame::Video) -> Option<DmabufFrame> {
        if !Source::is_hardware(decoded) {
            return None;
        }
        match DmabufFrame::export(decoded) {
            Ok(mapped)
                if self
                    .importable
                    .contains(&(mapped.fourcc, mapped.modifier())) =>
            {
                Some(mapped)
            }
            Ok(mapped) => {
                log::debug!(
                    "compositor cannot import fourcc {:#x} modifier {:#x}",
                    mapped.fourcc,
                    mapped.modifier()
                );
                None
            }
            Err(e) => {
                log::debug!("{e:#}");
                None
            }
        }
    }
}

/// `decoded` as a still just covering `screen`.
fn still_of(converter: &mut Converter, decoded: &frame::Video, screen: Size) -> Result<ShmImage> {
    let mut still = ShmImage::new(cover_size(Source::frame_size(decoded), screen))?;
    converter.convert(decoded, &mut still)?;
    Ok(still)
}
