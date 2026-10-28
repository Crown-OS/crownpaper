//! VA-API hardware decoding glue that ffmpeg-next does not wrap.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{self, NonNull};

use ffmpeg_next::ffi::{
    AV_PIX_FMT_FLAG_HWACCEL, AVBufferRef, AVCodecContext, AVHWDeviceType, AVPixelFormat,
    av_buffer_ref, av_buffer_unref, av_hwdevice_ctx_create, av_pix_fmt_desc_get,
};

/// Surfaces the decoder may hand out beyond its own reference frames: the
/// player queue plus the buffers the compositor is still scanning out.
const EXTRA_HW_FRAMES: i32 = 8;

/// A VA-API device on one render node.
pub struct HwDevice(NonNull<AVBufferRef>);

impl HwDevice {
    pub fn open(render_node: &Path) -> Option<Self> {
        let node = CString::new(render_node.as_os_str().as_bytes()).ok()?;
        let mut device = ptr::null_mut();
        // SAFETY: valid out-pointer and NUL-terminated device path; on
        // failure nothing is allocated.
        let status = unsafe {
            av_hwdevice_ctx_create(
                &mut device,
                AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                node.as_ptr(),
                ptr::null_mut(),
                0,
            )
        };
        if status < 0 {
            log::info!(
                "no VA-API on {}: video decodes in software",
                render_node.display()
            );
            return None;
        }
        NonNull::new(device).map(Self)
    }

    /// Makes `codec` decode on this device, falling back to software for
    /// codecs or profiles the hardware lacks.
    ///
    /// # Safety
    ///
    /// `codec` must be a valid, not yet opened codec context.
    pub unsafe fn attach(&self, codec: *mut AVCodecContext) {
        // SAFETY: forwarded to the caller; the codec takes its own reference.
        unsafe {
            (*codec).hw_device_ctx = av_buffer_ref(self.0.as_ptr());
            (*codec).get_format = Some(prefer_vaapi);
            (*codec).extra_hw_frames = EXTRA_HW_FRAMES;
        }
    }
}

impl Drop for HwDevice {
    fn drop(&mut self) {
        let mut device = self.0.as_ptr();
        // SAFETY: releases the reference taken in `open`.
        unsafe { av_buffer_unref(&mut device) };
    }
}

unsafe extern "C" fn prefer_vaapi(
    _codec: *mut AVCodecContext,
    offered: *const AVPixelFormat,
) -> AVPixelFormat {
    // SAFETY: FFmpeg passes a list terminated by AV_PIX_FMT_NONE.
    let offered = unsafe {
        let len = (0..)
            .take_while(|&i| *offered.add(i) != AVPixelFormat::AV_PIX_FMT_NONE)
            .count();
        std::slice::from_raw_parts(offered, len)
    };
    let is_software = |format: &&AVPixelFormat| {
        // SAFETY: every listed format has a descriptor.
        let descriptor = unsafe { av_pix_fmt_desc_get(**format) };
        // SAFETY: checked non-null before the dereference.
        !descriptor.is_null()
            && unsafe { (*descriptor).flags } & AV_PIX_FMT_FLAG_HWACCEL as u64 == 0
    };
    offered
        .iter()
        .find(|&&format| format == AVPixelFormat::AV_PIX_FMT_VAAPI)
        .or_else(|| offered.iter().find(is_software))
        .copied()
        .unwrap_or(AVPixelFormat::AV_PIX_FMT_NONE)
}
