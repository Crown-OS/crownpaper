//! Pixels in shared memory.
//!
//! A decoded wallpaper lives in a memfd from the moment it is decoded: the
//! compositor maps the same pages as a `wl_shm` buffer, and the GPU upload for
//! a transition reads them too, so no image is ever copied between the two.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::ptr::NonNull;

use rustix::fs::{MemfdFlags, fstat, ftruncate, memfd_create};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};

use crate::util::cover::Size;

/// Bytes per pixel of the layout every image uses: B, G, R, X in memory,
/// i.e. `wl_shm` XRGB8888 and wgpu `Bgra8UnormSrgb`.
pub const BYTES_PER_PIXEL: u32 = 4;

pub struct ShmImage {
    fd: OwnedFd,
    pixels: NonNull<u8>,
    size: Size,
}

// SAFETY: the mapping is owned exclusively by this value and freed on drop;
// shared access only hands out `&[u8]`.
unsafe impl Send for ShmImage {}
unsafe impl Sync for ShmImage {}

impl ShmImage {
    pub fn new(size: Size) -> io::Result<Self> {
        let fd = memfd_create("crownpaper", MemfdFlags::CLOEXEC)?;
        ftruncate(&fd, byte_len(size)? as u64)?;
        Self::map(fd, size)
    }

    /// Adopts an image another process wrote into `fd`.
    pub fn from_fd(fd: OwnedFd, size: Size) -> io::Result<Self> {
        let file_len = u64::try_from(fstat(&fd)?.st_size).unwrap_or(0);
        if file_len < byte_len(size)? as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "image fd is too small",
            ));
        }
        Self::map(fd, size)
    }

    fn map(fd: OwnedFd, size: Size) -> io::Result<Self> {
        let byte_len = byte_len(size)?;
        // SAFETY: a shared mapping within the file's verified length; the
        // ShmImage owns it until drop.
        let mapping = unsafe {
            mmap(
                std::ptr::null_mut(),
                byte_len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                &fd,
                0,
            )?
        };
        let pixels =
            NonNull::new(mapping.cast()).ok_or_else(|| io::Error::other("mmap returned null"))?;
        Ok(Self { fd, pixels, size })
    }

    pub fn filled(size: Size, bgrx: [u8; 4]) -> io::Result<Self> {
        let mut image = Self::new(size)?;
        image
            .pixels_mut()
            .chunks_exact_mut(BYTES_PER_PIXEL as usize)
            .for_each(|pixel| pixel.copy_from_slice(&bgrx));
        Ok(image)
    }

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn stride(&self) -> u32 {
        self.size.width * BYTES_PER_PIXEL
    }

    pub fn byte_len(&self) -> usize {
        self.stride() as usize * self.size.height as usize
    }

    pub fn pixels(&self) -> &[u8] {
        // SAFETY: the mapping spans byte_len bytes for as long as self lives.
        unsafe { std::slice::from_raw_parts(self.pixels.as_ptr(), self.byte_len()) }
    }

    pub fn pixels_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and &mut self guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.pixels.as_ptr(), self.byte_len()) }
    }
}

fn byte_len(size: Size) -> io::Result<usize> {
    match size.width as usize * size.height as usize * BYTES_PER_PIXEL as usize {
        0 => Err(io::Error::new(io::ErrorKind::InvalidInput, "empty image")),
        len => Ok(len),
    }
}

impl AsFd for ShmImage {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl Drop for ShmImage {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly the region mapped in `new`.
        if let Err(e) = unsafe { munmap(self.pixels.as_ptr().cast(), self.byte_len()) } {
            log::warn!("munmap failed: {e}");
        }
    }
}
