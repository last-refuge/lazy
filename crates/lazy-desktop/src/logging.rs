//! The desktop's log: `tracing` to the terminal as before, and to `logs/lazy.log` in the profile
//! folder, rotated by size (D24).
//!
//! The filter comes from `LAZY_LOG`, else `RUST_LOG`, else `lazy=info`. Events before the profile
//! folder is known are kept (up to a limit) and written when it is. Failing to open or write the
//! file never stops the program: logging to the terminal goes on, and the failure is one line on
//! it. The rotation is written here rather than taken from a crate: it is a few dozen lines, the
//! rules (the size, how many files) are this program's, and the tests can reach all of it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// The log file's name; older ones are `lazy.log.1`, `lazy.log.2`.
pub const FILE_NAME: &str = "lazy.log";
/// The size at which a file is closed and a new one started: about 5 MiB.
pub const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
/// How many files are kept, the newest included.
pub const KEEP_FILES: usize = 3;
/// What is held while the folder is not known yet.
const MAX_PENDING_BYTES: usize = 64 * 1024;

struct Inner {
    dir: Option<PathBuf>,
    file: Option<File>,
    size: u64,
    max_bytes: u64,
    keep: usize,
    pending: Vec<u8>,
    /// The file could not be used; do not try on every event.
    failed: bool,
}

/// A log file with size rotation. Cheap to clone; every clone writes to the same file.
#[derive(Clone)]
pub struct RotatingLog {
    inner: Arc<Mutex<Inner>>,
}

impl Default for RotatingLog {
    fn default() -> Self {
        Self::with_limits(MAX_FILE_BYTES, KEEP_FILES)
    }
}

impl RotatingLog {
    pub fn with_limits(max_bytes: u64, keep: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                dir: None,
                file: None,
                size: 0,
                max_bytes: max_bytes.max(1),
                keep: keep.max(1),
                pending: Vec::new(),
                failed: false,
            })),
        }
    }

    /// Starts writing into `dir` (created if need be), after what was held until now.
    pub fn attach(&self, dir: &Path) -> io::Result<()> {
        let mut inner = self.lock();
        fs::create_dir_all(dir)?;
        inner.dir = Some(dir.to_path_buf());
        inner.failed = false;
        inner.open()?;
        let pending = std::mem::take(&mut inner.pending);
        if !pending.is_empty() {
            inner.write_bytes(&pending);
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while logging must not stop logging for everyone else.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_all(&self, bytes: &[u8]) {
        self.lock().write_bytes(bytes);
    }
}

impl Inner {
    fn path(&self, index: usize) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        Some(if index == 0 {
            dir.join(FILE_NAME)
        } else {
            dir.join(format!("{FILE_NAME}.{index}"))
        })
    }

    fn open(&mut self) -> io::Result<()> {
        let Some(path) = self.path(0) else {
            return Ok(());
        };
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        self.size = file.metadata().map_or(0, |meta| meta.len());
        self.file = Some(file);
        Ok(())
    }

    /// `lazy.log` becomes `lazy.log.1`, that `.2`, and so on; what would pass the last kept is
    /// deleted.
    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        let last = self.keep - 1;
        if let Some(oldest) = self.path(last) {
            let _ = fs::remove_file(oldest);
        }
        for index in (0..last).rev() {
            if let (Some(from), Some(to)) = (self.path(index), self.path(index + 1))
                && from.exists()
            {
                fs::rename(from, to)?;
            }
        }
        self.size = 0;
        self.open()
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        if self.dir.is_none() {
            // The folder is not known yet: keep the start of the log for when it is.
            let room = MAX_PENDING_BYTES.saturating_sub(self.pending.len());
            self.pending
                .extend_from_slice(&bytes[..bytes.len().min(room)]);
            return;
        }
        if self.failed {
            return;
        }
        let result = (|| -> io::Result<()> {
            if self.file.is_none() {
                self.open()?;
            }
            // A line is never split across files, and an empty file takes whatever it is given.
            if self.size > 0 && self.size + bytes.len() as u64 > self.max_bytes {
                self.rotate()?;
            }
            if let Some(file) = self.file.as_mut() {
                file.write_all(bytes)?;
                self.size += bytes.len() as u64;
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.failed = true;
            self.file = None;
            // The terminal still has the log: say once that the file does not.
            eprintln!("Lazy: the log file stopped working ({error}); logging to the terminal only");
        }
    }
}

/// What `tracing` writes one event through.
pub struct LogWriter(RotatingLog);

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_all(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = self.0.lock().file.as_mut() {
            let _ = file.flush();
        }
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for RotatingLog {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter(self.clone())
    }
}

/// The filter: `LAZY_LOG`, else `RUST_LOG`, else the default; a filter that does not parse falls
/// back to the next.
pub fn filter_from(lazy_log: Option<&str>, rust_log: Option<&str>) -> EnvFilter {
    let mut filter = [lazy_log, rust_log]
        .into_iter()
        .flatten()
        .filter(|text| !text.trim().is_empty())
        .find_map(|text| EnvFilter::try_new(text).ok())
        .unwrap_or_else(|| EnvFilter::new("lazy=info"));
    for directive in ALWAYS_OFF {
        filter = filter.add_directive(directive.parse().expect("a fixed directive"));
    }
    filter
}

/// Directives added after the person's, so no filter can switch them on: `tungstenite` (the
/// WebSocket library) logs every frame it sends or receives at TRACE, message text included.
const ALWAYS_OFF: &[&str] = &["tungstenite=off", "tokio_tungstenite=off"];

/// Starts logging to the terminal; the file follows with [`RotatingLog::attach`].
pub fn init() -> RotatingLog {
    let log = RotatingLog::default();
    let filter = filter_from(
        std::env::var("LAZY_LOG").ok().as_deref(),
        std::env::var("RUST_LOG").ok().as_deref(),
    );
    let terminal = tracing_subscriber::fmt::layer().with_writer(io::stderr);
    let file = tracing_subscriber::fmt::layer()
        .with_writer(log.clone())
        .with_ansi(false);
    // A second `init` in one process (a test) is not an error worth stopping for.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(terminal)
        .with(file)
        .try_init();
    log
}

/// The folder the log files go in, under the profile.
pub fn log_dir(profile_dir: &Path) -> PathBuf {
    profile_dir.join("logs")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lazy-log-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read(dir: &Path, name: &str) -> String {
        fs::read_to_string(dir.join(name)).unwrap_or_default()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn lines_go_to_the_file_and_a_full_file_is_rotated() {
        let dir = temp_dir("rotate");
        let log = RotatingLog::with_limits(100, 3);
        log.attach(&dir).unwrap();
        // Ten-byte lines, ten to a file.
        for number in 0..45 {
            log.write_all(format!("line {number:04}\n").as_bytes());
        }
        assert_eq!(
            names(&dir),
            ["lazy.log", "lazy.log.1", "lazy.log.2"],
            "the newest three files are kept and no more"
        );
        for name in ["lazy.log", "lazy.log.1", "lazy.log.2"] {
            assert!(
                read(&dir, name).len() <= 100,
                "{name} stays within the limit"
            );
        }
        // The newest line is in the newest file; the oldest lines are gone; what is kept is
        // whole lines, in order, and contiguous.
        assert!(read(&dir, "lazy.log").contains("line 0044"));
        let kept = format!(
            "{}{}{}",
            read(&dir, "lazy.log.2"),
            read(&dir, "lazy.log.1"),
            read(&dir, "lazy.log")
        );
        assert!(!kept.contains("line 0000"), "the oldest was dropped");
        let numbers: Vec<u32> = kept
            .lines()
            .map(|line| line.trim_start_matches("line ").parse().unwrap())
            .collect();
        assert!(
            numbers.windows(2).all(|pair| pair[1] == pair[0] + 1),
            "{numbers:?}"
        );
        assert_eq!(numbers.last(), Some(&44));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_restart_appends_to_the_file_it_finds() {
        let dir = temp_dir("append");
        let first = RotatingLog::with_limits(1000, 3);
        first.attach(&dir).unwrap();
        first.write_all(b"before\n");
        let second = RotatingLog::with_limits(1000, 3);
        second.attach(&dir).unwrap();
        second.write_all(b"after\n");
        assert_eq!(read(&dir, "lazy.log"), "before\nafter\n");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_line_longer_than_the_limit_is_kept_whole_in_a_file_of_its_own() {
        let dir = temp_dir("long");
        let log = RotatingLog::with_limits(20, 2);
        log.attach(&dir).unwrap();
        log.write_all(b"short\n");
        log.write_all(b"a line that is much longer than the limit of twenty bytes\n");
        log.write_all(b"after\n");
        assert!(read(&dir, "lazy.log.1").contains("much longer"));
        assert_eq!(names(&dir).len(), 2, "two files are kept");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn what_was_logged_before_the_folder_was_known_is_written_when_it_is() {
        let dir = temp_dir("pending");
        let log = RotatingLog::with_limits(1000, 3);
        log.write_all(b"early\n");
        log.attach(&dir).unwrap();
        log.write_all(b"late\n");
        assert_eq!(read(&dir, "lazy.log"), "early\nlate\n");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failing_to_open_the_log_never_stops_anything() {
        // The "folder" is a file, so it cannot be created.
        let dir = temp_dir("fail");
        let blocker = dir.join("blocker");
        fs::write(&blocker, "x").unwrap();
        let log = RotatingLog::with_limits(1000, 3);
        assert!(log.attach(&blocker.join("logs")).is_err());
        // Writing afterwards is harmless, however often.
        for _ in 0..5 {
            log.write_all(b"still fine\n");
        }
        // A folder that stops being writable part way is survived too.
        let log = RotatingLog::with_limits(1000, 3);
        let logs = dir.join("logs");
        log.attach(&logs).unwrap();
        log.write_all(b"one\n");
        fs::remove_dir_all(&logs).unwrap();
        fs::write(&logs, "now a file").unwrap();
        // Rotation or reopening cannot work; nothing panics.
        for _ in 0..50 {
            log.write_all(b"two two two two two two two two two\n");
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_filter_comes_from_lazy_log_then_rust_log_then_the_default() {
        // What the person asked for, without the directives that are always added.
        let asked = |lazy: Option<&str>, rust: Option<&str>| {
            let text = filter_from(lazy, rust).to_string();
            let mut kept: Vec<&str> = text
                .split(',')
                .filter(|part| !ALWAYS_OFF.contains(part))
                .collect();
            kept.sort_unstable();
            kept.join(",")
        };
        assert_eq!(asked(Some("debug"), Some("trace")), "debug");
        assert_eq!(asked(None, Some("trace")), "trace");
        assert_eq!(asked(Some(""), Some("warn")), "warn");
        assert_eq!(asked(None, None), "lazy=info");
        // One that does not parse falls through to the next.
        assert_eq!(asked(Some("lazy=nonsense["), Some("warn")), "warn");
    }

    #[test]
    fn no_filter_can_switch_on_the_websocket_libraries_frame_logging() {
        for asked in [Some("trace"), Some("tungstenite=trace,trace"), None] {
            let text = filter_from(asked, None).to_string();
            for directive in ALWAYS_OFF {
                assert!(text.contains(directive), "{asked:?} gave {text}");
            }
        }
    }

    #[test]
    fn the_log_folder_is_under_the_profile() {
        assert_eq!(log_dir(Path::new("/p")), Path::new("/p/logs"));
    }
}
