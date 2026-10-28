//! Off-thread wallpaper decoding.
//!
//! A single worker thread decodes sequentially and ships screen-sized pixels
//! back into the daemon's event loop through a calloop channel. One decode at
//! a time keeps the peak memory of a bulk `preload` at a single image; `set`
//! requests jump the queue so a preload burst never delays a visible change.

mod jpeg;
mod probe;
mod raster;
mod resize;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;

use anyhow::{Context, Result};
use calloop::channel::Sender;

pub use probe::{StillFormat, WallpaperKind, probe};

use crate::video::{self, ImportableFormats, OpenedVideo};
use crownpaper::shm::ShmImage;
use crownpaper::util::cover::Size;
use crownpaper::util::mapped_file::MappedFile;

pub enum Decoded {
    Still(ShmImage),
    Video(OpenedVideo),
}

pub struct DecodeResult {
    pub path: PathBuf,
    pub result: Result<Decoded, String>,
}

pub struct Job {
    pub path: PathBuf,
    pub kind: WallpaperKind,
    /// Largest output, in physical pixels: images are decoded to just cover it.
    pub screen: Size,
    /// Render node for hardware video decoding.
    pub video_device: Option<PathBuf>,
    /// Buffer formats the compositor can import, for video frames.
    pub importable: ImportableFormats,
}

#[derive(Clone, Copy)]
pub enum Priority {
    /// The user is waiting to see this wallpaper — decode it next.
    Display,
    /// Cache warming; decode after any pending display work.
    Preload,
}

/// Handle to the decode worker. Dropping it shuts the worker down.
pub struct Decoder {
    queue: Arc<JobQueue>,
}

struct JobQueue {
    jobs: Mutex<Jobs>,
    ready: Condvar,
}

#[derive(Default)]
struct Jobs {
    pending: VecDeque<Job>,
    closed: bool,
}

impl JobQueue {
    fn lock(&self) -> MutexGuard<'_, Jobs> {
        self.jobs.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Decoder {
    pub fn spawn(results: Sender<DecodeResult>) -> Result<Self> {
        let queue = Arc::new(JobQueue {
            jobs: Mutex::new(Jobs::default()),
            ready: Condvar::new(),
        });
        let worker_queue = Arc::clone(&queue);
        thread::Builder::new()
            .name("crownpaper-decoder".into())
            .spawn(move || worker(&worker_queue, &results))
            .context("spawning the decode worker thread")?;
        Ok(Self { queue })
    }

    pub fn enqueue(&self, job: Job, priority: Priority) {
        let mut jobs = self.queue.lock();
        match priority {
            Priority::Display => jobs.pending.push_front(job),
            Priority::Preload => jobs.pending.push_back(job),
        }
        drop(jobs);
        self.queue.ready.notify_one();
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.queue.lock().closed = true;
        self.queue.ready.notify_one();
    }
}

fn worker(queue: &JobQueue, results: &Sender<DecodeResult>) {
    while let Some(job) = next_job(queue) {
        let result = decode(&job).map_err(|e| format!("{e:#}"));
        if results
            .send(DecodeResult {
                path: job.path,
                result,
            })
            .is_err()
        {
            return;
        }
    }
}

fn next_job(queue: &JobQueue) -> Option<Job> {
    let mut jobs = queue.lock();
    loop {
        if jobs.closed {
            return None;
        }
        if let Some(job) = jobs.pending.pop_front() {
            return Some(job);
        }
        jobs = queue
            .ready
            .wait(jobs)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

fn decode(job: &Job) -> Result<Decoded> {
    let path = &job.path;
    log::debug!("decoding {} as {:?}", path.display(), job.kind);
    let still = |format| -> Result<Decoded> {
        let bytes =
            MappedFile::open(path).with_context(|| format!("opening {}", path.display()))?;
        let image = match format {
            StillFormat::Jpeg => jpeg::decode(&bytes, job.screen),
            StillFormat::Raster(format) => raster::decode(&bytes, format, job.screen),
        };
        Ok(Decoded::Still(
            image.with_context(|| format!("decoding {}", path.display()))?,
        ))
    };
    match job.kind {
        WallpaperKind::Still(format) => still(format),
        WallpaperKind::Video => {
            video::open(
                path,
                job.screen,
                job.video_device.as_deref(),
                &job.importable,
            )
        }
        .map(Decoded::Video)
        .with_context(|| format!("opening video {}", path.display())),
    }
}
