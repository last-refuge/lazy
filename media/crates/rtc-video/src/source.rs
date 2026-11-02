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
    /// A camera: the one called `name` when it is there and opens, otherwise the first that
    /// opens and gives a picture (webcams before raw laptop sensors).
    Camera { name: Option<String> },
    /// A moving pattern on a background of one color (for tests, and machines without a camera).
    TestPattern { color: [u8; 3] },
}

impl VideoInput {
    /// Opens the source. Call it on the thread that will read it.
    pub fn open(&self) -> Result<Box<dyn VideoSource>, SourceError> {
        match self {
            VideoInput::Camera { name } => open_camera(name.as_deref()),
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

/// The cameras to choose from, by name: one entry per name (a webcam's infrared or metadata
/// devices share its name), without raw sensor feeds. Opening a camera by one of these names tries
/// every device of that name. Empty without camera support.
pub fn cameras() -> Vec<String> {
    #[cfg(feature = "camera")]
    {
        camera::names()
    }
    #[cfg(not(feature = "camera"))]
    {
        Vec::new()
    }
}

#[cfg(not(feature = "camera"))]
fn open_camera(_name: Option<&str>) -> Result<Box<dyn VideoSource>, SourceError> {
    Err(SourceError::Unsupported)
}

#[cfg(feature = "camera")]
fn open_camera(name: Option<&str>) -> Result<Box<dyn VideoSource>, SourceError> {
    camera::Camera::open(name).map(|camera| Box::new(camera) as Box<dyn VideoSource>)
}

#[cfg(feature = "camera")]
mod camera {
    use nokhwa::pixel_format::RgbFormat;
    use nokhwa::utils::{
        ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
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

    /// See [`super::cameras`].
    pub fn names() -> Vec<String> {
        let cameras = match nokhwa::query(ApiBackend::Auto) {
            Ok(cameras) => cameras,
            Err(e) => {
                tracing::warn!(error = %e, "could not list cameras");
                return Vec::new();
            }
        };
        let mut names: Vec<String> = Vec::new();
        for info in cameras {
            let name = info.human_name();
            if !is_raw_sensor(&name) && !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }

    impl Camera {
        /// Opens the camera called `preferred` if it opens, otherwise the first that does.
        pub fn open(preferred: Option<&str>) -> Result<Camera, SourceError> {
            if !permitted() {
                return Err(SourceError::Camera(
                    "Lazy is not allowed to use the camera".into(),
                ));
            }
            let mut cameras = nokhwa::query(ApiBackend::Auto).map_err(error)?;
            if cameras.is_empty() {
                return Err(SourceError::NoCamera);
            }
            // The chosen camera first. Raw sensor feeds (built-in laptop cameras behind an image
            // processor) last: they list as cameras but give no usable picture without libcamera
            // or PipeWire.
            cameras.sort_by_key(|info| {
                let name = info.human_name();
                (Some(name.as_str()) != preferred, is_raw_sensor(&name))
            });
            let mut first_failure = None;
            for info in &cameras {
                let name = info.human_name();
                match Self::open_one(info.index()) {
                    Ok(camera) => {
                        tracing::info!(camera = %name, "camera chosen");
                        return Ok(camera);
                    }
                    Err(e) => {
                        tracing::info!(camera = %name, error = %e, "camera skipped");
                        first_failure.get_or_insert(format!("{name}: {e}"));
                    }
                }
            }
            let failure = first_failure.unwrap_or_default();
            Err(SourceError::Camera(if cameras.len() == 1 {
                failure
            } else {
                format!("none of the {} cameras opened ({failure})", cameras.len())
            }))
        }

        fn open_one(index: &CameraIndex) -> Result<Camera, SourceError> {
            let index = match index {
                CameraIndex::Index(i) => CameraIndex::Index(*i),
                CameraIndex::String(s) => CameraIndex::String(s.clone()),
            };
            // Open in whatever format the camera offers first, then choose from its list
            // ourselves: nokhwa's own "closest" request only matches the exact size asked for
            // (it looks up frame rates for the requested size, not the closest one it found), so
            // a camera without 640x480 in that pixel format failed to open at all.
            let any = RequestedFormat::new::<RgbFormat>(RequestedFormatType::None);
            let mut camera = nokhwa::Camera::new(index, any).map_err(error)?;
            match camera.compatible_camera_formats() {
                Ok(formats) => {
                    if let Some(format) = choose_format(&formats) {
                        let exact =
                            RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(format));
                        if let Err(e) = camera.set_camera_requset(exact) {
                            tracing::warn!(?format, error = %e, "camera kept its own format");
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "could not list the camera's formats"),
            }
            camera.open_stream().map_err(error)?;
            tracing::info!(format = ?camera.camera_format(), "camera open");
            Ok(Camera { camera })
        }
    }

    /// Whether a camera's name says it is a raw sensor feed rather than a picture: the capture
    /// nodes of Intel's IPU6 and later image processors ("Intel IPU6 ISYS Capture 0"), which need
    /// libcamera or Intel's camera software to make an image.
    pub(super) fn is_raw_sensor(name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        name.contains("isys") || name.contains(" ipu")
    }

    /// The pixel formats a camera frame can be turned into a picture from.
    const DECODABLE: [FrameFormat; 5] = [
        FrameFormat::YUYV,
        FrameFormat::NV12,
        FrameFormat::RAWRGB,
        FrameFormat::RAWBGR,
        FrameFormat::MJPEG,
    ];

    /// Below this many frames a second, video looks broken; a format that reaches it wins over
    /// any size.
    const MIN_FRAME_RATE: u32 = 15;

    /// The camera format to send from: one that reaches [`MIN_FRAME_RATE`], then the size
    /// nearest the send size (a little larger beats smaller, as frames are scaled down to fit),
    /// then an uncompressed format (cheaper than decoding JPEG), then the frame rate nearest
    /// [`FRAME_RATE`].
    pub(super) fn choose_format(formats: &[CameraFormat]) -> Option<CameraFormat> {
        let wanted = i64::from(SEND_WIDTH) * i64::from(SEND_HEIGHT);
        formats
            .iter()
            .filter(|f| DECODABLE.contains(&f.format()) && f.frame_rate() > 0)
            .min_by_key(|f| {
                let area = i64::from(f.width()) * i64::from(f.height());
                let size_cost = if area >= wanted {
                    area - wanted
                } else {
                    (wanted - area) * 4
                };
                let format_rank = DECODABLE
                    .iter()
                    .position(|d| *d == f.format())
                    .unwrap_or(DECODABLE.len());
                (
                    f.frame_rate() < MIN_FRAME_RATE,
                    size_cost,
                    format_rank,
                    f.frame_rate().abs_diff(FRAME_RATE),
                )
            })
            .copied()
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

    #[cfg(test)]
    mod tests {
        use nokhwa::utils::Resolution;

        use super::*;

        fn format(width: u32, height: u32, frame: FrameFormat, fps: u32) -> CameraFormat {
            CameraFormat::new(Resolution::new(width, height), frame, fps)
        }

        #[test]
        fn raw_sensor_feeds_are_told_from_webcams() {
            assert!(is_raw_sensor("Intel IPU6 ISYS Capture 0"));
            assert!(is_raw_sensor("Intel IPU7 ISYS Capture 12"));
            assert!(!is_raw_sensor("Logitech BRIO"));
            assert!(
                !is_raw_sensor("Intel MIPI Camera"),
                "the processed loopback works"
            );
            assert!(!is_raw_sensor("Integrated Camera: Integrated C"));
        }

        #[test]
        fn a_camera_without_640x480_yuyv_still_gets_a_format() {
            // An HD webcam: YUYV only at sizes too slow for video, MJPEG at 30.
            let formats = [
                format(1280, 720, FrameFormat::YUYV, 10),
                format(1920, 1080, FrameFormat::YUYV, 5),
                format(1280, 720, FrameFormat::MJPEG, 30),
                format(1920, 1080, FrameFormat::MJPEG, 30),
            ];
            assert_eq!(
                choose_format(&formats),
                Some(format(1280, 720, FrameFormat::MJPEG, 30))
            );
        }

        #[test]
        fn the_send_size_uncompressed_wins_when_offered() {
            let formats = [
                format(1280, 720, FrameFormat::MJPEG, 30),
                format(640, 480, FrameFormat::MJPEG, 30),
                format(640, 480, FrameFormat::YUYV, 30),
                format(320, 240, FrameFormat::YUYV, 30),
            ];
            assert_eq!(
                choose_format(&formats),
                Some(format(640, 480, FrameFormat::YUYV, 30))
            );
        }

        #[test]
        fn a_little_larger_beats_smaller_and_slow_formats_lose() {
            let formats = [
                format(320, 240, FrameFormat::NV12, 30),
                format(848, 480, FrameFormat::NV12, 30),
                format(640, 480, FrameFormat::NV12, 7),
            ];
            assert_eq!(
                choose_format(&formats),
                Some(format(848, 480, FrameFormat::NV12, 30))
            );
        }

        #[test]
        fn only_slow_or_undecodable_formats_still_choose_something_decodable() {
            let formats = [
                format(640, 480, FrameFormat::GRAY, 30),
                format(1280, 720, FrameFormat::YUYV, 10),
            ];
            assert_eq!(
                choose_format(&formats),
                Some(format(1280, 720, FrameFormat::YUYV, 10))
            );
            assert_eq!(choose_format(&[]), None);
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
