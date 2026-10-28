//! Spawning and handshaking with the helper process.

use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use crownpaper::shm::ShmImage;
use crownpaper::util::cover::Size;
use crownpaper::video_wire::{FromHelper, ToHelper, recv, send, socketpair};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::{FdFlags, fcntl_setfd};

const HELPER_NAME: &str = "crownpaper-video";

/// A video opened by the decode worker: its first frame as a poster for the
/// transition, plus the helper waiting to stream the rest.
pub struct OpenedVideo {
    pub poster: ShmImage,
    pub helper: VideoHelper,
}

/// A running helper. Dropping it kills the process.
pub struct VideoHelper {
    process: Child,
    pub(super) socket: Arc<OwnedFd>,
}

pub fn open(
    path: &Path,
    screen: Size,
    render_node: Option<&Path>,
    importable: &[(u32, u64)],
) -> Result<OpenedVideo> {
    let (daemon_end, helper_end) = socketpair().context("creating the helper socket")?;
    let helper = VideoHelper {
        process: spawn(&helper_end)?,
        socket: Arc::new(daemon_end),
    };
    drop(helper_end);
    let socket = helper.socket.as_fd();
    let start = ToHelper::Start {
        path: path.to_string_lossy().into_owned(),
        screen,
        render_node: render_node.map(|node| node.to_string_lossy().into_owned()),
        importable: importable.to_vec(),
    };
    send(socket, &start, &[]).context("starting the video helper")?;
    let poster = match recv::<FromHelper>(socket)? {
        Some((FromHelper::Poster { size }, mut fds)) => {
            let fd = fds.pop().context("poster without pixels")?;
            ShmImage::from_fd(fd, size)?
        }
        Some((FromHelper::Failed(reason), _)) => bail!(reason),
        Some(_) => bail!("unexpected first message from the video helper"),
        None => bail!("the video helper exited"),
    };
    fcntl_setfl(socket, fcntl_getfl(socket)? | OFlags::NONBLOCK)?;
    Ok(OpenedVideo { poster, helper })
}

fn spawn(helper_end: &OwnedFd) -> Result<Child> {
    let raw = helper_end.as_raw_fd();
    let mut command = Command::new(helper_path());
    command.arg(raw.to_string());
    // SAFETY: only an async-signal-safe fcntl runs between fork and exec, to
    // let the helper inherit its end of the socket.
    unsafe {
        command.pre_exec(move || {
            let inherited = std::os::fd::BorrowedFd::borrow_raw(raw);
            fcntl_setfd(inherited, FdFlags::empty()).map_err(std::io::Error::from)
        });
    }
    command
        .spawn()
        .with_context(|| format!("spawning {HELPER_NAME}"))
}

/// Next to our own executable, so a build tree and an install both work.
fn helper_path() -> PathBuf {
    std::env::current_exe()
        .map(|exe| exe.with_file_name(HELPER_NAME))
        .ok()
        .filter(|path| path.exists())
        .unwrap_or_else(|| PathBuf::from(HELPER_NAME))
}

impl Drop for VideoHelper {
    fn drop(&mut self) {
        let _ = self.process.kill();
        if let Err(e) = self.process.wait() {
            log::warn!("reaping the video helper: {e}");
        }
    }
}
