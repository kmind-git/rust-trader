//! quickfixgo-style per-session FIX logs. Two files per session under the
//! configured log directory (FileLogPath):
//!
//! - `{prefix}.messages.current.log` — every raw inbound ("in") and outbound
//!   ("out") message, heartbeats included; direction prefix added by us,
//!   quickfixgo's file log does not mark it
//! - `{prefix}.event.current.log` — session state-machine events, wording
//!   follows quickfixgo's event strings
//!
//! File names follow quickfixgo's prefix scheme
//! `{BeginString}-{SenderCompID}-{TargetCompID}`. Logging=N yields a disabled
//! handle whose writes are no-ops.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// logging configuration shared by the acceptor and the initiator
#[derive(Clone, Debug)]
pub struct LogConfig {
    /// Logging=Y/N from the settings file (default Y)
    pub enabled: bool,
    /// FileLogPath from the settings file; each binary has its own default
    pub dir: String,
}

impl LogConfig {
    pub fn from_config(config: &super::config::FixConfig, default_dir: &str) -> LogConfig {
        LogConfig {
            enabled: config.get_or("Logging", "Y").eq_ignore_ascii_case("Y"),
            dir: config.get_or("FileLogPath", default_dir),
        }
    }
}

/// cloneable handle shared between a session's reader and writer threads
#[derive(Clone)]
pub struct SessionLog {
    inner: Arc<SessionLogInner>,
}

struct SessionLogInner {
    messages: Mutex<Option<File>>,
    event: Mutex<Option<File>>,
}

impl SessionLog {
    /// create the two log files eagerly; on failure the caller gets the error
    /// and should fall back to `disabled` (after a log::warn)
    pub fn new(dir: &str, begin_string: &str, sender: &str, target: &str) -> std::io::Result<SessionLog> {
        std::fs::create_dir_all(dir)?;
        let prefix = filename_prefix(begin_string, sender, target);
        let open = |name: &str| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(PathBuf::from(dir).join(name))
        };
        let messages = open(&format!("{prefix}.messages.current.log"))?;
        let event = open(&format!("{prefix}.event.current.log"))?;
        Ok(SessionLog {
            inner: Arc::new(SessionLogInner {
                messages: Mutex::new(Some(messages)),
                event: Mutex::new(Some(event)),
            }),
        })
    }

    /// disabled handle: Logging=N, every write is a no-op
    pub fn disabled() -> SessionLog {
        SessionLog {
            inner: Arc::new(SessionLogInner {
                messages: Mutex::new(None),
                event: Mutex::new(None),
            }),
        }
    }

    /// an inbound raw message (tag=value with SOH separators)
    pub fn incoming(&self, raw: &str) {
        self.write_message("in", raw);
    }

    /// an outbound raw message
    pub fn outgoing(&self, raw: &str) {
        self.write_message("out", raw);
    }

    /// a session state-machine event
    pub fn event(&self, text: &str) {
        if let Some(file) = self.inner.event.lock().unwrap().as_mut() {
            let _ = writeln!(file, "{} {}", timestamp(), text);
        }
    }

    fn write_message(&self, direction: &str, raw: &str) {
        if let Some(file) = self.inner.messages.lock().unwrap().as_mut() {
            let _ = writeln!(file, "{} {} {}", timestamp(), direction, raw);
        }
    }
}

/// quickfixgo file prefix: "FIX.4.4-SENDER-TARGET" (log/file/file_util.go)
pub fn filename_prefix(begin_string: &str, sender: &str, target: &str) -> String {
    format!("{}-{}-{}", begin_string, sender, target)
}

/// log line prefix: Beijing time (UTC+8, fixed offset) with microseconds.
/// Log readability only — FIX field timestamps (52/60) stay UTC via
/// frame::utc_timestamp / codec::transact_time.
fn timestamp() -> String {
    let beijing = chrono::FixedOffset::east_opt(8 * 3600).expect("UTC+8 is a valid offset");
    chrono::Utc::now()
        .with_timezone(&beijing)
        .format("%Y-%m-%d %H:%M:%S%.6f")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filename_prefix() {
        assert_eq!(filename_prefix("FIX.4.2", "GOX", "CLIENT"), "FIX.4.2-GOX-CLIENT");
    }

    #[test]
    fn test_log_files_written() {
        let dir = std::env::temp_dir().join(format!("gotrader-log-test-{}-{}", std::process::id(), chrono::Utc::now().timestamp_nanos_opt().unwrap()));
        let log = SessionLog::new(dir.to_str().unwrap(), "FIX.4.2", "GOX", "CLIENT").unwrap();
        log.incoming("8=FIX.4.2\x0135=A\x0110=000\x01");
        log.outgoing("8=FIX.4.2\x0135=0\x0110=000\x01");
        log.event("Received logon request");
        drop(log);

        let prefix = filename_prefix("FIX.4.2", "GOX", "CLIENT");
        let messages = std::fs::read_to_string(dir.join(format!("{prefix}.messages.current.log"))).unwrap();
        assert!(messages.contains(" in 8=FIX.4.2\x0135=A\x0110=000\x01"));
        assert!(messages.contains(" out 8=FIX.4.2\x0135=0\x0110=000\x01"));
        let event = std::fs::read_to_string(dir.join(format!("{prefix}.event.current.log"))).unwrap();
        assert!(event.contains("Received logon request"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_disabled_log_writes_nothing() {
        let dir = std::env::temp_dir().join(format!("gotrader-log-disabled-{}-{}", std::process::id(), chrono::Utc::now().timestamp_nanos_opt().unwrap()));
        let log = SessionLog::new(dir.to_str().unwrap(), "FIX.4.2", "GOX", "X").unwrap();
        drop(log);
        let disabled = SessionLog::disabled();
        disabled.incoming("8=FIX.4.2\x0110=000\x01");
        disabled.event("no-op");
        // disabled handle never creates anything; dir only has the first session's files
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(entries.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
