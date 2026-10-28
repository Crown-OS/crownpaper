//! Read-only file mappings, so compressed wallpapers are decoded straight
//! from the page cache instead of being copied onto the heap first.

use std::fs::File;
use std::io;
use std::ops::Deref;
use std::path::Path;
use std::ptr::NonNull;

use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};

pub struct MappedFile {
    bytes: NonNull<u8>,
    len: usize,
}

impl MappedFile {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = usize::try_from(file.metadata()?.len()).map_err(io::Error::other)?;
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "empty file"));
        }
        // SAFETY: a private read-only mapping; the fd may close afterwards.
        // Wallpapers are not expected to be truncated while being decoded.
        let mapping = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ,
                MapFlags::PRIVATE,
                &file,
                0,
            )?
        };
        let bytes =
            NonNull::new(mapping.cast()).ok_or_else(|| io::Error::other("mmap returned null"))?;
        Ok(Self { bytes, len })
    }
}

impl Deref for MappedFile {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: the mapping covers len bytes until drop.
        unsafe { std::slice::from_raw_parts(self.bytes.as_ptr(), self.len) }
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what `open` mapped.
        if let Err(e) = unsafe { munmap(self.bytes.as_ptr().cast(), self.len) } {
            log::warn!("munmap failed: {e}");
        }
    }
}
