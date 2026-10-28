//! Exporting VA-API surfaces as dmabufs the compositor can sample directly.

use std::os::fd::{BorrowedFd, RawFd};
use std::ptr::NonNull;

use anyhow::{Result, bail};
use ffmpeg_next::ffi::{
    AV_HWFRAME_MAP_READ, AVDRMFrameDescriptor, AVFrame, AVPixelFormat, av_frame_alloc,
    av_frame_free, av_hwframe_map,
};
use ffmpeg_next::frame;

use crownpaper::util::cover::Size;

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*code)
}

const NV12: u32 = fourcc(b"NV12");
const P010: u32 = fourcc(b"P010");
const R8: u32 = fourcc(b"R8  ");
const GR88: u32 = fourcc(b"GR88");
const R16: u32 = fourcc(b"R16 ");
const GR1616: u32 = fourcc(b"GR32");

pub struct Plane {
    fd: RawFd,
    pub offset: u32,
    pub stride: u32,
    pub modifier: u64,
}

/// A decoded frame mapped to DRM PRIME. The mapping keeps the VA surface and
/// its exported fds alive, so it must outlive every `wl_buffer` built on it.
pub struct DmabufFrame {
    mapped: NonNull<AVFrame>,
    pub size: Size,
    pub fourcc: u32,
    pub planes: Vec<Plane>,
}

impl DmabufFrame {
    pub fn export(frame: &frame::Video) -> Result<Self> {
        // SAFETY: av_frame_alloc returns an owned frame or null.
        let Some(mapped) = NonNull::new(unsafe { av_frame_alloc() }) else {
            bail!("out of memory mapping a video frame");
        };
        let mut exported = Self {
            mapped,
            size: Size::new(frame.width(), frame.height()),
            fourcc: 0,
            planes: Vec::with_capacity(2),
        };
        // SAFETY: `mapped` is a fresh frame we own and `frame` is a valid
        // VA-API frame; on success data[0] points at a DRM descriptor that
        // lives as long as the mapped frame.
        let descriptor = unsafe {
            (*mapped.as_ptr()).format = AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            let status =
                av_hwframe_map(mapped.as_ptr(), frame.as_ptr(), AV_HWFRAME_MAP_READ as i32);
            if status < 0 {
                bail!(
                    "mapping a VA-API surface to DRM failed ({})",
                    ffmpeg_next::Error::from(status)
                );
            }
            &*(*mapped.as_ptr()).data[0].cast::<AVDRMFrameDescriptor>()
        };
        let layers = &descriptor.layers[..descriptor.nb_layers.clamp(0, 4) as usize];
        exported.fourcc = match layers {
            [single] => single.format,
            [luma, chroma] => match (luma.format, chroma.format) {
                (R8, GR88) => NV12,
                (R16, GR1616) => P010,
                other => bail!("unsupported DRM layer pair {other:?}"),
            },
            _ => bail!("unsupported DRM layout with {} layers", layers.len()),
        };
        exported.planes = layers
            .iter()
            .flat_map(|layer| &layer.planes[..layer.nb_planes.clamp(0, 4) as usize])
            .map(|plane| {
                let object = &descriptor.objects[plane.object_index.clamp(0, 3) as usize];
                Plane {
                    fd: object.fd,
                    offset: plane.offset as u32,
                    stride: plane.pitch as u32,
                    modifier: object.format_modifier,
                }
            })
            .collect();
        Ok(exported)
    }

    pub fn modifier(&self) -> u64 {
        self.planes.first().map_or(0, |plane| plane.modifier)
    }
}

impl Plane {
    pub fn fd(&self) -> BorrowedFd<'_> {
        // SAFETY: the fd belongs to the DRM descriptor of the mapped frame
        // that owns this plane, and stays open until that frame is freed.
        unsafe { BorrowedFd::borrow_raw(self.fd) }
    }
}

impl Drop for DmabufFrame {
    fn drop(&mut self) {
        let mut mapped = self.mapped.as_ptr();
        // SAFETY: frees the frame allocated in `export`, closing its fds.
        unsafe { av_frame_free(&mut mapped) };
    }
}
