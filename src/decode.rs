//! Off-thread wallpaper decoding.
//!
//! A single worker thread decodes images sequentially and ships raw RGBA
//! bytes back into the daemon's event loop through a calloop channel. One
//! decode at a time keeps the peak memory of a bulk `preload` at a single
//! frame buffer; `set` requests jump the queue so a preload burst never
//! delays a visible wallpaper change.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use anyhow::{ensure, Context, Result};
use crownshell::calloop::channel::Sender;

/// Raw pixels ready for `Queue::write_texture`.
pub struct DecodedImage {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub struct DecodeResult {
    pub path: PathBuf,
    pub result: Result<DecodedImage, String>,
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
    paths: VecDeque<PathBuf>,
    closed: bool,
}

impl Decoder {
    pub fn spawn(results: Sender<DecodeResult>) -> Self {
        let queue = Arc::new(JobQueue {
            jobs: Mutex::new(Jobs::default()),
            ready: Condvar::new(),
        });
        let worker_queue = Arc::clone(&queue);
        thread::Builder::new()
            .name("crownpaper-decoder".into())
            .spawn(move || worker(&worker_queue, &results))
            .expect("spawning the decode worker thread");
        Self { queue }
    }

    pub fn enqueue(&self, path: PathBuf, priority: Priority) {
        let mut jobs = self.queue.jobs.lock().expect("decode queue poisoned");
        match priority {
            Priority::Display => jobs.paths.push_front(path),
            Priority::Preload => jobs.paths.push_back(path),
        }
        drop(jobs);
        self.queue.ready.notify_one();
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        if let Ok(mut jobs) = self.queue.jobs.lock() {
            jobs.closed = true;
        }
        self.queue.ready.notify_one();
    }
}

fn worker(queue: &JobQueue, results: &Sender<DecodeResult>) {
    loop {
        let path = {
            let mut jobs = queue.jobs.lock().expect("decode queue poisoned");

            loop {
                if jobs.closed {
                    return;
                }
                if let Some(path) = jobs.paths.pop_front() {
                    break path;
                }
                jobs = queue.ready.wait(jobs).expect("decode queue poisoned");
            }
        };
        let result = decode(&path).map_err(|e| format!("{e:#}"));
        if results.send(DecodeResult { path, result }).is_err() {
            return; // daemon is gone
        }
    }
}

/// Cheap validity check used to acknowledge or reject a request without
/// decoding pixels: opens the file and sniffs the container format.
pub fn probe(path: &Path) -> Result<()> {
    let reader = image::ImageReader::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("sniffing format of {}", path.display()))?;
    ensure!(
        reader.format().is_some(),
        "{}: unrecognized image format",
        path.display()
    );
    Ok(())
}

fn decode(path: &Path) -> Result<DecodedImage> {
    let reader = image::ImageReader::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("sniffing format of {}", path.display()))?;
    if let Some(format) = reader.format() {
        log::debug!("decoding {} as {format:?}", path.display());
    }
    let rgba = reader
        .decode()
        .with_context(|| format!("decoding {}", path.display()))?
        .to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(DecodedImage {
        rgba: rgba.into_raw(),
        width,
        height,
    })
}
