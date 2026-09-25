//! QuickFIX/Go-style per-session FIX logs.
//!
//! Two files are kept per session under `FileLogPath`:
//! `messages.current.log` records every raw inbound/outbound message and
//! `event.current.log` records session events. The `in`/`out` marker and the
//! fixed UTC+8 event timestamp are explicit project extensions; FIX fields
//! such as SendingTime(52) remain UTC.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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
    /// Build logging settings from a resolved `[SESSION]` map. This is the
    /// only constructor used by the binaries, so invalid boolean values are
    /// reported instead of silently disabling logging.
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

    /// Compatibility helper for callers that intentionally inspect DEFAULT
    /// values. New startup code should resolve a session first.
    pub fn from_config(config: &FixConfig, default_dir: &str) -> Result<LogConfig, ConfigError> {
        Self::from_settings(&config.defaults, default_dir)
    }
}

/// Cloneable handle shared between a session's reader and writer threads.
#[derive(Clone)]
pub struct SessionLog {
    inner: Arc<SessionLogInner>,
}

struct SessionLogInner {
    messages: Mutex<Option<File>>,
    event: Mutex<Option<File>>,
}

impl SessionLog {
    /// Create the two log files eagerly. The caller can warn and use
    /// [`SessionLog::disabled`] if the directory or files cannot be opened.
    pub fn new(
        dir: &str,
        begin_string: &str,
        sender: &str,
        target: &str,
    ) -> std::io::Result<SessionLog> {
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

    /// Disabled handle: `Logging=N`, every write is a no-op.
    pub fn disabled() -> SessionLog {
        SessionLog {
            inner: Arc::new(SessionLogInner {
                messages: Mutex::new(None),
                event: Mutex::new(None),
            }),
        }
    }

    /// Record an inbound raw message. The message bytes are written without
    /// UTF-8 reconstruction, preserving SOH and any valid ASCII data exactly.
    pub fn incoming(&self, raw: &str) {
        self.write_message("in", raw.as_bytes());
    }

    /// Record an outbound raw message.
    pub fn outgoing(&self, raw: &str) {
        self.write_message("out", raw.as_bytes());
    }

    /// Record bytes retained by the frame decoder after a malformed or
    /// truncated frame. Hex encoding is used so arbitrary bytes cannot inject
    /// line breaks into the messages log while remaining lossless.
    pub fn incoming_bytes(&self, raw: &[u8]) {
        let mut encoded = String::with_capacity(raw.len() * 2);
        for byte in raw {
            use std::fmt::Write as _;
            let _ = write!(&mut encoded, "{byte:02X}");
        }
        self.write_message("in-hex", encoded.as_bytes());
    }

    /// Record a session state-machine event.
    pub fn event(&self, text: &str) {
        let mut guard = self.inner.event.lock().unwrap();
        if let Some(file) = guard.as_mut() {
            let result = writeln!(file, "{} {}", timestamp(), text);
            warn_write_failure("event", result);
        }
    }

    fn write_message(&self, direction: &str, raw: &[u8]) {
        let mut guard = self.inner.messages.lock().unwrap();
        if let Some(file) = guard.as_mut() {
            let result = (|| {
                write!(file, "{} {} ", timestamp(), direction)?;
                file.write_all(raw)?;
                file.write_all(b"\n")
            })();
            warn_write_failure("message", result);
        }
    }
}

fn warn_write_failure(kind: &str, result: std::io::Result<()>) {
    if let Err(error) = result {
        log::warn!("FIX {kind} log write failed: {error}");
    }
}

/// Filename prefix following QuickFIX/Go's shape while preventing a CompID
/// or BeginString from introducing a path separator/control character.
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

/// Log line timestamp: Beijing time (UTC+8, fixed offset) with microseconds.
/// This is a readability extension; FIX field timestamps stay UTC.
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

    fn test_dir(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-runs")
            .join("fix-log-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
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
        let log = SessionLog::new(dir.to_str().unwrap(), "FIX.4.2", "GOX", "CLIENT").unwrap();
        log.incoming("8=FIX.4.2\x0135=A\x0110=000\x01");
        log.outgoing("8=FIX.4.2\x0135=0\x0110=000\x01");
        log.incoming_bytes(&[0, 0xFF, b'\n']);
        log.event("Received logon request");
        drop(log);

        let prefix = filename_prefix("FIX.4.2", "GOX", "CLIENT");
        let messages = std::fs::read(dir.join(format!("{prefix}.messages.current.log"))).unwrap();
        assert!(messages
            .windows(b" in 8=FIX.4.2\x0135=A\x0110=000\x01\n".len())
            .any(|window| window == b" in 8=FIX.4.2\x0135=A\x0110=000\x01\n"));
        assert!(messages
            .windows(b" out 8=FIX.4.2\x0135=0\x0110=000\x01\n".len())
            .any(|window| window == b" out 8=FIX.4.2\x0135=0\x0110=000\x01\n"));
        let text = String::from_utf8(messages).unwrap();
        assert!(text.contains(" in-hex 00FF0A\n"));
        let event =
            std::fs::read_to_string(dir.join(format!("{prefix}.event.current.log"))).unwrap();
        assert!(event.contains("Received logon request"));
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
