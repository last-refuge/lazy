//! Where a client's video comes from. A source is opened on the thread that reads it (cameras are
//! tied to the thread on some systems) and read in a loop; [`VideoSource::read`] blocks until the
//! next frame.

use std::time::{Duration, Instant};

use crate::Frame;

/// The size frames are sent at, at most: enough for a tile, light on the network.
pub const SEND_WIDTH: u32 = 640;
pub const SEND_HEIGHT: u32 = 480;
/// The frame rate asked of cameras and kept by the test pattern.
pub const FRAME_RATE: u32 = 20;

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("no camera was found")]
    NoCamera,
    #[error("the camera could not be opened: {0}")]
    Camera(String),
    #[error("this build has no camera support")]
    Unsupported,
}

/// A stream of frames.
pub trait VideoSource {
    /// The next frame; blocks until there is one.
    fn read(&mut self) -> Result<Frame, SourceError>;
}

/// Which source to open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VideoInput {
    /// The system's first camera.
    Camera,
    /// A moving pattern on a background of one color (for tests, and machines without a camera).
    TestPattern { color: [u8; 3] },
}

impl VideoInput {
    /// Opens the source. Call it on the thread that will read it.
    pub fn open(&self) -> Result<Box<dyn VideoSource>, SourceError> {
        match self {
            VideoInput::Camera => open_camera(),
            VideoInput::TestPattern { color } => Ok(Box::new(TestPattern::new(*color))),
        }
    }
}

/// 320 by 240 frames of `color` with a white bar sweeping across, at [`FRAME_RATE`].
pub struct TestPattern {
    color: [u8; 3],
    frame: u32,
    next: Instant,
}

impl TestPattern {
    pub const WIDTH: u32 = 320;
    pub const HEIGHT: u32 = 240;

    pub fn new(color: [u8; 3]) -> TestPattern {
        TestPattern {
            color,
            frame: 0,
            next: Instant::now(),
        }
    }

    /// The frame `n` (no waiting).
    pub fn frame(color: [u8; 3], n: u32) -> Frame {
        let mut frame = Frame::solid(Self::WIDTH, Self::HEIGHT, color);
        let bar = Self::WIDTH / 10;
        let start = (n * 4) % (Self::WIDTH - bar);
        for y in 0..Self::HEIGHT {
            for x in start..start + bar {
                let at = ((y * Self::WIDTH + x) * 4) as usize;
                frame.bgra[at..at + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
        frame
    }
}

impl VideoSource for TestPattern {
    fn read(&mut self) -> Result<Frame, SourceError> {
        let now = Instant::now();
        if self.next > now {
            std::thread::sleep(self.next - now);
        }
        self.next = Instant::now().max(self.next) + Duration::from_secs(1) / FRAME_RATE;
        self.frame += 1;
        Ok(Self::frame(self.color, self.frame))
    }
}

#[cfg(not(feature = "camera"))]
fn open_camera() -> Result<Box<dyn VideoSource>, SourceError> {
    Err(SourceError::Unsupported)
}

#[cfg(feature = "camera")]
fn open_camera() -> Result<Box<dyn VideoSource>, SourceError> {
    camera::Camera::open().map(|camera| Box::new(camera) as Box<dyn VideoSource>)
}

#[cfg(feature = "camera")]
mod camera {
    use nokhwa::pixel_format::RgbFormat;
    use nokhwa::utils::{
        ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
        Resolution,
    };

    use super::{FRAME_RATE, SEND_HEIGHT, SEND_WIDTH, SourceError, VideoSource};
    use crate::Frame;

    pub struct Camera {
        camera: nokhwa::Camera,
    }

    fn error(e: impl std::fmt::Display) -> SourceError {
        SourceError::Camera(e.to_string())
    }

    /// On macOS, asks for permission (the first time, the system asks the person) and waits for
    /// the answer.
    #[cfg(target_os = "macos")]
    fn permitted() -> bool {
        if nokhwa::nokhwa_check() {
            return true;
        }
        let (sender, receiver) = std::sync::mpsc::channel();
        nokhwa::nokhwa_initialize(move |granted| {
            sender.send(granted).ok();
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(60))
            .unwrap_or(false)
    }

    #[cfg(not(target_os = "macos"))]
    fn permitted() -> bool {
        true
    }

    impl Camera {
        pub fn open() -> Result<Camera, SourceError> {
            if !permitted() {
                return Err(SourceError::Camera(
                    "Lazy is not allowed to use the camera".into(),
                ));
            }
            let cameras = nokhwa::query(ApiBackend::Auto).map_err(error)?;
            let first = cameras.first().ok_or(SourceError::NoCamera)?;
            let index = match first.index() {
                CameraIndex::Index(i) => CameraIndex::Index(*i),
                CameraIndex::String(s) => CameraIndex::String(s.clone()),
            };
            let wanted =
                RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(CameraFormat::new(
                    Resolution::new(SEND_WIDTH, SEND_HEIGHT),
                    FrameFormat::YUYV,
                    FRAME_RATE,
                )));
            let mut camera = nokhwa::Camera::new(index, wanted).map_err(error)?;
            camera.open_stream().map_err(error)?;
            tracing::info!(format = ?camera.camera_format(), "camera open");
            Ok(Camera { camera })
        }
    }

    impl VideoSource for Camera {
        fn read(&mut self) -> Result<Frame, SourceError> {
            let buffer = self.camera.frame().map_err(error)?;
            let frame = if buffer.source_frame_format() == FrameFormat::MJPEG {
                let mut decoder = zune_jpeg::JpegDecoder::new(buffer.buffer());
                let rgb = decoder.decode().map_err(error)?;
                let (width, height) = decoder.dimensions().ok_or_else(|| error("no size"))?;
                Frame::from_rgb(width as u32, height as u32, &rgb)
            } else {
                let image = buffer.decode_image::<RgbFormat>().map_err(error)?;
                Frame::from_rgb(image.width(), image.height(), image.as_raw())
            };
            Ok(frame.fit_within(SEND_WIDTH, SEND_HEIGHT))
        }
    }

    impl Drop for Camera {
        fn drop(&mut self) {
            self.camera.stop_stream().ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_pattern_moves_and_keeps_its_color() {
        let a = TestPattern::frame([10, 200, 30], 1);
        let b = TestPattern::frame([10, 200, 30], 20);
        assert_ne!(a, b, "the bar moved");
        assert_eq!(a.pixel(TestPattern::WIDTH - 1, 0), [10, 200, 30]);
    }

    #[test]
    fn the_test_pattern_keeps_its_frame_rate() {
        let mut source = VideoInput::TestPattern { color: [0, 0, 0] }.open().unwrap();
        let started = Instant::now();
        for _ in 0..5 {
            source.read().unwrap();
        }
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(150), "{elapsed:?}");
    }
}
