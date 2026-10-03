//! A session log for unexpected exits.
//!
//! The Apps menu launches inkmark under `systemd-run --quiet`, so stderr
//! never reaches the journal and a clean-looking quit leaves no trace.
//! This file records why the process ended: a close request, the event
//! loop returning, or a panic. When stderr is not a terminal, it is also
//! pointed at the same file, so a Wayland library message is kept too.

use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static SESSION_LOG: SessionLog = SessionLog;

static LOG: Mutex<Option<File>> = Mutex::new(None);

/// Opens the session log and, outside a terminal, keeps stderr there too.
/// A missing home directory leaves logging off; the app still runs.
pub fn init() {
    let Some(path) = log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    // A menu launch has no terminal, and systemd-run --quiet drops stderr.
    // Duplicate the log onto stderr before the window exists, so a protocol
    // error on the way out is in the file. The e2e harness captures stderr
    // itself and leaves this alone, so Wayland debug does not mix into the
    // session lines.
    if std::env::var_os("INKMARK_E2E").is_none()
        && !std::io::stderr().is_terminal()
        && let Ok(copy) = file.try_clone()
    {
        let _ = rustix::stdio::dup2_stderr(copy);
    }
    if let Ok(mut guard) = LOG.lock() {
        *guard = Some(file);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    install_loggers();
    line(&format!(
        "start pid={} version={} log={} args={args:?}",
        std::process::id(),
        env!("CARGO_PKG_VERSION"),
        path.display()
    ));
    install_panic_hook();
    spawn_heartbeat();
}

/// Opens `path` for tests. Does not install a panic hook or touch stderr.
#[cfg(test)]
pub fn init_at(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    if let Ok(mut guard) = LOG.lock() {
        *guard = Some(file);
    }
    line("start test");
}

pub fn line(message: &str) {
    let Ok(mut guard) = LOG.lock() else {
        return;
    };
    let Some(file) = guard.as_mut() else {
        return;
    };
    let _ = writeln!(file, "{} {message}", stamp());
    let _ = file.flush();
}

/// Seconds between `alive` lines. `INKMARK_HEARTBEAT_SECS` overrides the default.
pub fn heartbeat_every() -> Duration {
    let secs = std::env::var("INKMARK_HEARTBEAT_SECS")
        .ok()
        .and_then(|text| text.parse().ok())
        .filter(|secs: &u64| *secs > 0)
        .unwrap_or(10);
    Duration::from_secs(secs)
}

/// `INKMARK_E2E_QUIT_AFTER` seconds: the e2e harness asks the window to close.
pub fn quit_after() -> Option<Duration> {
    std::env::var("INKMARK_E2E_QUIT_AFTER")
        .ok()
        .and_then(|text| text.parse().ok())
        .filter(|secs: &u64| *secs > 0)
        .map(Duration::from_secs)
}

fn log_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("INKMARK_LOG") {
        return Some(PathBuf::from(path));
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")));
    state.map(|dir| dir.join("inkmark/session.log"))
}

/// Warn and error records from `log` and `tracing` (winit's Wayland
/// dispatch failure is a tracing error). Debug stays off: Wayland protocol
/// traffic is enabled only by the e2e harness, on stderr.
fn install_loggers() {
    let _ = log::set_logger(&SESSION_LOG);
    log::set_max_level(log::LevelFilter::Warn);
    let _ = tracing::subscriber::set_global_default(SessionTrace);
}

fn spawn_heartbeat() {
    let every = heartbeat_every();
    let _ = std::thread::Builder::new()
        .name("inkmark-session".into())
        .spawn(move || {
            loop {
                std::thread::sleep(every);
                line("process alive");
            }
        });
}

struct SessionLog;

impl log::Log for SessionLog {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            line(&format!(
                "log {} {}: {}",
                record.level(),
                record.target(),
                record.args()
            ));
        }
    }

    fn flush(&self) {}
}

struct SessionTrace;

impl tracing::Subscriber for SessionTrace {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() <= tracing::Level::WARN
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::WARN)
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() > tracing::Level::WARN {
            return;
        }
        let mut message = String::new();
        event.record(&mut FieldVisit(&mut message));
        line(&format!(
            "tracing {} {}: {message}",
            event.metadata().level(),
            event.metadata().target()
        ));
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

struct FieldVisit<'a>(&'a mut String);

impl tracing::field::Visit for FieldVisit<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        self.0.push_str(&format!("{}={value:?}", field.name()));
    }
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        line(&format!("panic {info}\n{backtrace}"));
        previous(info);
    }));
}

fn stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Howard Hinnant's civil-from-days, UTC.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_appended_with_a_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.log");
        init_at(&path);
        line("hello from the test");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("start test"), "{text}");
        assert!(text.contains("hello from the test"), "{text}");
        assert!(text.contains('T') && text.contains('Z'), "{text}");
    }

    #[test]
    fn unix_epoch_is_the_civil_date() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
    }
}
