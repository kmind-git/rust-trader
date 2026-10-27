//! QuickFIX/J-style per-session FIX logs, rotated by the local calendar date.
//!
//! Each `FileLogPath/YYYY-MM-DD` directory contains a session's
//! `messages.log` (unchanged raw inbound/outbound messages) and `event.log`.
//! Lines use `YYYYMMDD-HH:MM:SS.mmm: ` with local time; FIX fields 52/60
//! continue to use UTC. This logger is independent of `RUST_LOG`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, FixedOffset, Local, NaiveDate};

use super::config::{ConfigError, FixConfig, SessionSettings};

/// Logging configuration shared by the acceptor and initiator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogConfig {
    /// `Logging=Y/N` from the settings file (default Y).
    pub enabled: bool,
    /// `FileLogPath` from the settings file; each binary supplies its own
    /// default when the setting is absent.
    pub dir: String,
}

impl LogConfig {
    /// Build logging settings from a resolved `[SESSION]` map.
    pub fn from_settings(
        settings: &SessionSettings,
        default_dir: &str,
    ) -> Result<LogConfig, ConfigError> {
        let dir = settings.get_or("FileLogPath", default_dir);
        if dir.trim().is_empty() {
            return Err(ConfigError::Unsupported {
                key: "FileLogPath".to_string(),
                value: dir,
                message: "log path must not be empty".to_string(),
            });
        }
        Ok(LogConfig {
            enabled: settings.bool("Logging", true)?,
            dir,
        })
    }

    /// Compatibility helper for callers that intentionally inspect DEFAULT.
    pub fn from_config(config: &FixConfig, default_dir: &str) -> Result<LogConfig, ConfigError> {
        Self::from_settings(&config.defaults, default_dir)
    }
}

/// Cloneable handle shared between a session's reader and writer threads.
#[derive(Clone)]
pub struct SessionLog {
    inner: Arc<Mutex<Option<DailyFiles>>>,
}

struct DailyFiles {
    dir: PathBuf,
    prefix: String,
    date: NaiveDate,
    retention_days: u64,
    messages: File,
    event: File,
}

impl DailyFiles {
    fn open(
        dir: PathBuf,
        prefix: String,
        date: NaiveDate,
        retention_days: u64,
    ) -> std::io::Result<Self> {
        let (messages, event) = open_files(&dir, &prefix, date)?;
        let files = Self {
            dir,
            prefix,
            date,
            retention_days,
            messages,
            event,
        };
        files.prune();
        Ok(files)
    }

    fn rotate(&mut self, date: NaiveDate) -> std::io::Result<()> {
        if self.date == date {
            return Ok(());
        }
        // Open both files before switching. A failure does not silently write
        // a new day's records into yesterday's log.
        let (messages, event) = open_files(&self.dir, &self.prefix, date)?;
        self.messages = messages;
        self.event = event;
        self.date = date;
        self.prune();
        Ok(())
    }

    fn prune(&self) {
        if let Err(error) =
            prune_session_logs(&self.dir, &self.prefix, self.date, self.retention_days)
        {
            log::warn!("FIX log retention cleanup failed: {error}");
        }
    }
}

fn open_files(dir: &Path, prefix: &str, date: NaiveDate) -> std::io::Result<(File, File)> {
    let daily_dir = dir.join(date.format("%Y-%m-%d").to_string());
    std::fs::create_dir_all(&daily_dir)?;
    let open = |suffix: &str| {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(daily_dir.join(format!("{prefix}.{suffix}.log")))
    };
    Ok((open("messages")?, open("event")?))
}

/// Resolve `FIX_LOG_RETENTION_DAYS` from runtime settings (environment first);
/// zero explicitly disables deletion.
/// Startup validation can call this before opening any listening ports.
pub fn retention_days() -> std::io::Result<u64> {
    let root = crate::runtime::installation_root()?;
    let settings = crate::runtime::RuntimeSettings::load(&root)?;
    settings.days("FIX_LOG_RETENTION_DAYS").map(u64::from)
}

/// Delete only this session's two known log files from expired date
/// directories. Never recursively remove a directory or follow symlinks.
fn prune_session_logs(
    dir: &Path,
    prefix: &str,
    today: NaiveDate,
    days: u64,
) -> std::io::Result<()> {
    if days == 0 {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(date) = NaiveDate::parse_from_str(name, "%Y-%m-%d") else {
            continue;
        };
        if date.format("%Y-%m-%d").to_string() != name {
            continue;
        }
        let age = today.signed_duration_since(date).num_days();
        if age < 0 || (age as u64) < days {
            continue;
        }
        for suffix in ["messages", "event"] {
            let path = entry.path().join(format!("{prefix}.{suffix}.log"));
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        // Other sessions or user files keep the directory alive.
        let _ = std::fs::remove_dir(entry.path());
    }
    Ok(())
}

impl SessionLog {
    /// Eagerly create today's two files and apply the retention policy.
    pub fn new(
        dir: &str,
        begin_string: &str,
        sender: &str,
        target: &str,
    ) -> std::io::Result<SessionLog> {
        Self::new_at(
            PathBuf::from(dir),
            filename_prefix(begin_string, sender, target),
            Local::now().date_naive(),
            retention_days()?,
        )
    }

    fn new_at(
        dir: PathBuf,
        prefix: String,
        date: NaiveDate,
        days: u64,
    ) -> std::io::Result<SessionLog> {
        Ok(SessionLog {
            inner: Arc::new(Mutex::new(Some(DailyFiles::open(dir, prefix, date, days)?))),
        })
    }

    /// Disabled handle: `Logging=N`, every write is a no-op.
    pub fn disabled() -> SessionLog {
        SessionLog {
            inner: Arc::new(Mutex::new(None)),
        }
    }

    /// Write received bytes unchanged, including all ignored extension fields.
    pub fn incoming(&self, raw: &str) {
        self.write_record(false, raw.as_bytes(), Local::now().fixed_offset());
    }

    /// Record an outbound raw message.
    pub fn outgoing(&self, raw: &str) {
        self.write_record(false, raw.as_bytes(), Local::now().fixed_offset());
    }

    /// Preserve malformed/truncated frames losslessly as hex so arbitrary
    /// bytes cannot inject line breaks into the messages log.
    pub fn incoming_bytes(&self, raw: &[u8]) {
        let mut encoded = String::with_capacity(7 + raw.len() * 2);
        encoded.push_str("in-hex ");
        for byte in raw {
            use std::fmt::Write as _;
            let _ = write!(&mut encoded, "{byte:02X}");
        }
        self.write_record(false, encoded.as_bytes(), Local::now().fixed_offset());
    }

    /// Record a session state-machine event.
    pub fn event(&self, text: &str) {
        self.write_record(true, text.as_bytes(), Local::now().fixed_offset());
    }

    fn write_record(&self, event: bool, raw: &[u8], now: DateTime<FixedOffset>) {
        let mut guard = self.inner.lock().unwrap();
        if let Some(files) = guard.as_mut() {
            let result = (|| {
                files.rotate(now.date_naive())?;
                let file = if event {
                    &mut files.event
                } else {
                    &mut files.messages
                };
                write!(file, "{}: ", now.format("%Y%m%d-%H:%M:%S%.3f"))?;
                file.write_all(raw)?;
                file.write_all(b"\n")
            })();
            if let Err(error) = result {
                log::warn!(
                    "FIX {} log write failed: {error}",
                    if event { "event" } else { "message" }
                );
            }
        }
    }
}

/// Session filename prefix, with path separators/control characters removed.
pub fn filename_prefix(begin_string: &str, sender: &str, target: &str) -> String {
    format!(
        "{}-{}-{}",
        safe_filename_component(begin_string),
        safe_filename_component(sender),
        safe_filename_component(target)
    )
}

fn safe_filename_component(value: &str) -> String {
    let mut component: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if component.is_empty() || component == "." || component == ".." {
        component = "_".to_string();
    }
    component
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-runs")
            .join("fix-log-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn date(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
    }

    fn time(value: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(value).unwrap()
    }

    #[test]
    fn test_filename_prefix() {
        assert_eq!(
            filename_prefix("FIX.4.2", "GOX", "CLIENT"),
            "FIX.4.2-GOX-CLIENT"
        );
        assert_eq!(
            filename_prefix("FIX/4.2", "../GOX", "C\\CLIENT"),
            "FIX_4.2-.._GOX-C_CLIENT"
        );
    }

    #[test]
    fn test_log_files_written() {
        let dir = test_dir("written");
        let prefix = filename_prefix("FIX.4.2", "GOX", "CLIENT");
        let log = SessionLog::new_at(dir.clone(), prefix.clone(), date("2026-10-27"), 14).unwrap();
        let now = time("2026-10-27T09:30:01.123+05:30");
        let inbound = b"8=FIX.4.2\x0135=A\x019001=abc\x0110=000\x01";
        let outbound = b"8=FIX.4.2\x0135=0\x0110=000\x01";
        log.write_record(false, inbound, now);
        log.write_record(false, outbound, now);
        log.write_record(true, b"Received logon request", now);
        drop(log);

        let daily = dir.join("2026-10-27");
        let messages = std::fs::read(daily.join(format!("{prefix}.messages.log"))).unwrap();
        let mut expected = b"20261027-09:30:01.123: ".to_vec();
        expected.extend_from_slice(inbound);
        expected.extend_from_slice(b"\n20261027-09:30:01.123: ");
        expected.extend_from_slice(outbound);
        expected.push(b'\n');
        assert_eq!(messages, expected);
        assert_eq!(
            std::fs::read_to_string(daily.join(format!("{prefix}.event.log"))).unwrap(),
            "20261027-09:30:01.123: Received logon request\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_midnight_rotates_both_logs_using_local_date() {
        let dir = test_dir("midnight");
        let prefix = filename_prefix("FIX.4.2", "GOX", "CLIENT");
        let log = SessionLog::new_at(dir.clone(), prefix.clone(), date("2026-10-27"), 14).unwrap();
        log.write_record(false, b"before", time("2026-10-27T23:59:59.999-04:00"));
        log.write_record(true, b"after event", time("2026-10-28T00:00:00.000-04:00"));
        log.write_record(
            false,
            b"after message",
            time("2026-10-28T00:00:00.001-04:00"),
        );
        drop(log);
        assert_eq!(
            std::fs::read_to_string(
                dir.join("2026-10-27")
                    .join(format!("{prefix}.messages.log"))
            )
            .unwrap(),
            "20261027-23:59:59.999: before\n"
        );
        assert_eq!(
            std::fs::read_to_string(
                dir.join("2026-10-28")
                    .join(format!("{prefix}.messages.log"))
            )
            .unwrap(),
            "20261028-00:00:00.001: after message\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("2026-10-28").join(format!("{prefix}.event.log")))
                .unwrap(),
            "20261028-00:00:00.000: after event\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_rotation_failure_does_not_append_to_previous_day() {
        let dir = test_dir("rotation-failure");
        let prefix = filename_prefix("FIX.4.2", "GOX", "CLIENT");
        let log = SessionLog::new_at(dir.clone(), prefix.clone(), date("2026-10-27"), 0).unwrap();
        log.write_record(false, b"before", time("2026-10-27T23:59:59Z"));
        // A regular file prevents creation of tomorrow's date directory.
        std::fs::write(dir.join("2026-10-28"), b"obstacle").unwrap();
        log.write_record(
            false,
            b"must not enter yesterday",
            time("2026-10-28T00:00:00Z"),
        );
        drop(log);
        assert_eq!(
            std::fs::read_to_string(
                dir.join("2026-10-27")
                    .join(format!("{prefix}.messages.log"))
            )
            .unwrap(),
            "20261027-23:59:59.000: before\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_retention_only_removes_own_expired_files() {
        let dir = test_dir("retention");
        let prefix = filename_prefix("FIX.4.2", "GOX", "CLIENT");
        for name in [
            "2026-10-12",
            "2026-10-13",
            "2026-10-14",
            "2026-10-28",
            "not-a-date",
        ] {
            let daily = dir.join(name);
            std::fs::create_dir_all(&daily).unwrap();
            std::fs::write(daily.join(format!("{prefix}.messages.log")), "messages").unwrap();
            std::fs::write(daily.join(format!("{prefix}.event.log")), "event").unwrap();
        }
        std::fs::write(dir.join("2026-10-13/another.messages.log"), "other session").unwrap();
        std::fs::write(dir.join("2026-10-13/keep.txt"), "user file").unwrap();
        let log = SessionLog::new_at(dir.clone(), prefix.clone(), date("2026-10-27"), 14).unwrap();
        assert!(!dir.join("2026-10-12").exists());
        assert!(!dir
            .join("2026-10-13")
            .join(format!("{prefix}.messages.log"))
            .exists());
        assert!(!dir
            .join("2026-10-13")
            .join(format!("{prefix}.event.log"))
            .exists());
        assert!(dir.join("2026-10-13/another.messages.log").exists());
        assert!(dir.join("2026-10-13/keep.txt").exists());
        for name in ["2026-10-14", "2026-10-28", "not-a-date"] {
            assert!(dir
                .join(name)
                .join(format!("{prefix}.messages.log"))
                .exists());
        }
        drop(log);
        // Rotation applies retention again, including for an existing session.
        let log = SessionLog::new_at(dir.clone(), prefix.clone(), date("2026-10-27"), 14).unwrap();
        log.write_record(true, b"next day", time("2026-10-28T00:00:00Z"));
        assert!(!dir.join("2026-10-14").exists());
        drop(log);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_zero_retention_keeps_old_logs() {
        let dir = test_dir("keep-all");
        let prefix = filename_prefix("FIX.4.2", "GOX", "CLIENT");
        let old = dir.join("2000-01-01");
        std::fs::create_dir_all(&old).unwrap();
        let path = old.join(format!("{prefix}.messages.log"));
        std::fs::write(&path, "keep").unwrap();
        let log = SessionLog::new_at(dir.clone(), prefix, date("2026-10-27"), 0).unwrap();
        assert!(path.exists());
        drop(log);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_incoming_bytes_preserved_as_hex() {
        let dir = test_dir("hex");
        let prefix = filename_prefix("FIX.4.2", "GOX", "CLIENT");
        let log =
            SessionLog::new_at(dir.clone(), prefix.clone(), Local::now().date_naive(), 0).unwrap();
        log.incoming_bytes(&[0, 0xFF, b'\n']);
        drop(log);
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        let daily = entries[0].as_ref().unwrap().path();
        let messages =
            std::fs::read_to_string(daily.join(format!("{prefix}.messages.log"))).unwrap();
        assert!(messages.ends_with(": in-hex 00FF0A\n"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_disabled_log_writes_nothing() {
        let dir = test_dir("disabled");
        let disabled = SessionLog::disabled();
        disabled.incoming("8=FIX.4.2\x0110=000\x01");
        disabled.incoming_bytes(&[0, 1, 2]);
        disabled.event("no-op");
        assert!(!dir.exists());
    }

    #[test]
    fn test_log_config_rejects_bad_logging_value() {
        let mut settings = SessionSettings::default();
        settings
            .settings
            .insert("Logging".to_string(), "true".to_string());
        assert!(
            matches!(LogConfig::from_settings(&settings, "target/logs"), Err(ConfigError::Unsupported { key, .. }) if key == "Logging")
        );
    }
}
