//! The demux/decode loop over one video file.

use std::path::Path;

use anyhow::{Context, Result, bail};
use crownpaper::util::cover::Size;
use ffmpeg_next::format::{self, Pixel};
use ffmpeg_next::{Packet, Rational, codec, decoder, frame, media};

use crate::hw::HwDevice;

const FALLBACK_FPS: f64 = 30.0;

/// An endlessly looping stream of decoded frames with monotonic timestamps.
pub struct Source {
    input: format::context::Input,
    decoder: decoder::Video,
    stream_index: usize,
    time_base: f64,
    frame_seconds: f64,
    first_timestamp: Option<i64>,
    loop_offset: f64,
    last_time: f64,
}

impl Source {
    pub fn open(path: &Path, render_node: Option<&Path>) -> Result<Self> {
        let input = format::input(path).context("opening the container")?;
        let stream = input
            .streams()
            .best(media::Type::Video)
            .context("no video stream")?;
        let stream_index = stream.index();
        let time_base = f64::from(stream.time_base());
        let fps = rational_or(stream.avg_frame_rate(), FALLBACK_FPS);

        let mut context = codec::context::Context::from_parameters(stream.parameters())?;
        if let Some(device) = render_node.and_then(HwDevice::open) {
            // SAFETY: the context is valid and not opened yet.
            unsafe { device.attach(context.as_mut_ptr()) };
        }
        let decoder = context
            .decoder()
            .video()
            .context("opening the video decoder")?;
        Ok(Self {
            input,
            decoder,
            stream_index,
            time_base,
            frame_seconds: 1.0 / fps,
            first_timestamp: None,
            loop_offset: 0.0,
            last_time: 0.0,
        })
    }

    /// The next frame and its presentation time in seconds since the first
    /// frame, counting on across loops.
    pub fn next_frame(&mut self) -> Result<(frame::Video, f64)> {
        let mut restarts_without_frame = 0;
        loop {
            let mut frame = frame::Video::empty();
            match self.decoder.receive_frame(&mut frame) {
                Ok(()) => {
                    let time = self.time_of(&frame);
                    return Ok((frame, time));
                }
                Err(ffmpeg_next::Error::Eof) => {
                    restarts_without_frame += 1;
                    if restarts_without_frame > 1 {
                        bail!("the video has no decodable frames");
                    }
                    self.restart()?;
                    continue;
                }
                Err(ffmpeg_next::Error::Other { errno }) if errno == ffmpeg_next::error::EAGAIN => {
                }
                Err(e) => return Err(e).context("decoding a video frame"),
            }
            let mut packet = Packet::empty();
            match packet.read(&mut self.input) {
                Ok(()) if packet.stream() == self.stream_index => {
                    self.decoder
                        .send_packet(&packet)
                        .context("feeding the decoder")?;
                }
                Ok(()) => {}
                Err(ffmpeg_next::Error::Eof) => self.decoder.send_eof()?,
                Err(e) => return Err(e).context("reading the container"),
            }
        }
    }

    pub fn is_hardware(frame: &frame::Video) -> bool {
        frame.format() == Pixel::VAAPI
    }

    pub fn frame_size(frame: &frame::Video) -> Size {
        Size::new(frame.width(), frame.height())
    }

    fn time_of(&mut self, frame: &frame::Video) -> f64 {
        let time = match frame.timestamp() {
            Some(timestamp) => {
                let first = *self.first_timestamp.get_or_insert(timestamp);
                self.loop_offset + (timestamp - first) as f64 * self.time_base
            }
            None => self.last_time + self.frame_seconds,
        };
        self.last_time = time.max(self.last_time);
        self.last_time
    }

    fn restart(&mut self) -> Result<()> {
        self.input.seek(0, ..1).context("rewinding the video")?;
        self.decoder.flush();
        self.loop_offset = self.last_time + self.frame_seconds;
        Ok(())
    }
}

fn rational_or(value: Rational, fallback: f64) -> f64 {
    let value = f64::from(value);
    if value.is_finite() && value > 0.0 {
        value
    } else {
        fallback
    }
}
