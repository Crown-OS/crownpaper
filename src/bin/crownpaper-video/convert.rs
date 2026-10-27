//! Video frames to BGRX shared memory, for posters and snapshots, and for
//! playback when the compositor cannot take the decoder's buffers directly.

use anyhow::{Context, Result, bail};
use ffmpeg_next::ffi::{av_hwframe_transfer_data, sws_scale};
use ffmpeg_next::format::Pixel;
use ffmpeg_next::frame;
use ffmpeg_next::software::scaling::{self, Flags};

use crownpaper::shm::ShmImage;
use crownpaper::util::cover::Size;

#[derive(Default)]
pub struct Converter {
    scaler: Option<(Key, scaling::Context)>,
}

#[derive(PartialEq, Eq, Clone, Copy)]
struct Key {
    format: Pixel,
    source: Size,
    target: Size,
}

impl Converter {
    /// Scales `frame` into `output`, downloading it from the GPU first if it
    /// is a hardware frame.
    pub fn convert(&mut self, frame: &frame::Video, output: &mut ShmImage) -> Result<()> {
        let downloaded;
        let frame = if frame.format() == Pixel::VAAPI {
            downloaded = download(frame)?;
            &downloaded
        } else {
            frame
        };
        let key = Key {
            format: frame.format(),
            source: Size::new(frame.width(), frame.height()),
            target: output.size(),
        };
        if self
            .scaler
            .as_ref()
            .is_none_or(|(cached, _)| *cached != key)
        {
            let scaler = scaling::Context::get(
                key.format,
                key.source.width,
                key.source.height,
                Pixel::BGRA,
                key.target.width,
                key.target.height,
                Flags::BILINEAR,
            )
            .context("creating the video scaler")?;
            self.scaler = Some((key, scaler));
        }
        let Some((_, scaler)) = self.scaler.as_mut() else {
            bail!("video scaler missing");
        };
        let stride = output.stride() as i32;
        let destination = [
            output.pixels_mut().as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ];
        let strides = [stride, 0, 0, 0];
        // SAFETY: the scaler was configured for exactly this source format and
        // size, and the single destination plane spans target height × stride.
        let rows = unsafe {
            let source = frame.as_ptr();
            sws_scale(
                scaler.as_mut_ptr(),
                (*source).data.as_ptr().cast(),
                (*source).linesize.as_ptr(),
                0,
                key.source.height as i32,
                destination.as_ptr(),
                strides.as_ptr(),
            )
        };
        if rows <= 0 {
            bail!("scaling a video frame failed");
        }
        Ok(())
    }
}

fn download(frame: &frame::Video) -> Result<frame::Video> {
    let mut downloaded = frame::Video::empty();
    // SAFETY: both frames are valid; FFmpeg allocates the destination planes.
    let status = unsafe { av_hwframe_transfer_data(downloaded.as_mut_ptr(), frame.as_ptr(), 0) };
    if status < 0 {
        bail!(
            "downloading a hardware frame failed ({})",
            ffmpeg_next::Error::from(status)
        );
    }
    Ok(downloaded)
}
