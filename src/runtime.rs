//! Installation-relative defaults and daily application logs for all binaries.
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Packaged binaries live in ROOT/bin; Cargo builds locate their source root
/// from the executable's ancestors, never from the caller's working directory.
pub fn installation_root() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let parent = exe
        .parent()
        .ok_or_else(|| io::Error::other("executable has no parent"))?;
    if parent.file_name().is_some_and(|name| name == "bin") {
        return Ok(parent.parent().unwrap_or(parent).to_path_buf());
    }
    for candidate in parent.ancestors() {
        if candidate.join("Cargo.toml").is_file() && candidate.join("configs").is_dir() {
            return Ok(candidate.to_path_buf());
        }
    }
    Ok(parent.to_path_buf())
}

pub fn resolve(root: &Path, path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

pub struct RuntimeSettings {
    root: PathBuf,
    values: HashMap<String, String>,
}

impl RuntimeSettings {
    pub fn load(root: &Path) -> io::Result<Self> {
        let path = root.join("configs/runtime.env");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let mut values = HashMap::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=').ok_or_else(|| {
                io::Error::other(format!(
                    "{}:{}: expected KEY=value",
                    path.display(),
                    index + 1
                ))
            })?;
            let key = key.trim();
            if ![
                "RUST_LOG",
                "APP_LOG_PATH",
                "APP_LOG_RETENTION_DAYS",
                "FIX_LOG_RETENTION_DAYS",
                "REST_PORT",
                "START_TIMEOUT",
                "STOP_TIMEOUT",
            ]
            .contains(&key)
            {
                return Err(io::Error::other(format!(
                    "{}:{}: unknown runtime setting {key}",
                    path.display(),
                    index + 1
                )));
            }
            if values
                .insert(key.to_string(), value.trim().to_string())
                .is_some()
            {
                return Err(io::Error::other(format!("duplicate runtime setting {key}")));
            }
        }
        let settings = Self {
            root: root.to_path_buf(),
            values,
        };
        for key in ["APP_LOG_RETENTION_DAYS", "FIX_LOG_RETENTION_DAYS"] {
            settings.days(key)?;
        }
        Ok(settings)
    }

    pub fn value(&self, key: &str, default: &str) -> String {
        std::env::var(key)
            .ok()
            .or_else(|| self.values.get(key).cloned())
            .unwrap_or_else(|| default.to_string())
    }

    pub fn days(&self, key: &str) -> io::Result<u32> {
        self.value(key, "14")
            .parse()
            .map_err(|_| io::Error::other(format!("{key} must be a nonnegative integer")))
    }

    pub fn init_logging(&self, binary: &str) -> io::Result<()> {
        let log_path = self.value("APP_LOG_PATH", "logs/app");
        if log_path.trim().is_empty() {
            return Err(io::Error::other("APP_LOG_PATH must not be empty"));
        }
        let writer = DailyWriter::new(
            resolve(&self.root, log_path),
            binary,
            self.days("APP_LOG_RETENTION_DAYS")?,
        )?;
        let mut builder = env_logger::Builder::new();
        builder.parse_filters(&self.value("RUST_LOG", "info"));
        builder.format(|buf, record| {
            writeln!(
                buf,
                "{} {:<5} {}: {}",
                chrono::Local::now().format("%Y%m%d-%H:%M:%S%.3f"),
                record.level(),
                record.target(),
                record.args()
            )
        });
        builder.target(env_logger::Target::Pipe(Box::new(writer)));
        builder.try_init().map_err(io::Error::other)
    }
}

pub fn check_writable_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    let probe = path.join(format!(".rust-trader-write-{}", std::process::id()));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    drop(file);
    std::fs::remove_file(probe)
}

/// Fatal startup errors remain visible when filtering is off or stderr is
/// redirected, and also enter the daily application log when initialized.
pub fn fatal(message: &str) -> ! {
    log::error!("{message}");
    if !log::log_enabled!(log::Level::Error) || !io::stderr().is_terminal() {
        eprintln!("{message}");
    }
    std::process::exit(1)
}

struct DailyWriter {
    dir: PathBuf,
    binary: String,
    date: chrono::NaiveDate,
    retention: u32,
    file: File,
}

impl DailyWriter {
    fn new(dir: PathBuf, binary: &str, retention: u32) -> io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let date = chrono::Local::now().date_naive();
        let file = Self::open(&dir, binary, date)?;
        let writer = Self {
            dir,
            binary: binary.to_string(),
            date,
            retention,
            file,
        };
        writer.prune()?;
        Ok(writer)
    }

    fn open(dir: &Path, binary: &str, date: chrono::NaiveDate) -> io::Result<File> {
        OpenOptions::new()
            .append(true)
            .create(true)
            .open(dir.join(format!("{binary}-{date}.log")))
    }

    fn prune(&self) -> io::Result<()> {
        if self.retention == 0 {
            return Ok(());
        }
        let prefix = format!("{}-", self.binary);
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(day) = name
                .strip_prefix(&prefix)
                .and_then(|s| s.strip_suffix(".log"))
            else {
                continue;
            };
            if let Ok(day) = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d") {
                if self.date.signed_duration_since(day).num_days() >= i64::from(self.retention) {
                    std::fs::remove_file(entry.path())?;
                }
            }
        }
        Ok(())
    }
}

impl Write for DailyWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let today = chrono::Local::now().date_naive();
        if today != self.date {
            let file = Self::open(&self.dir, &self.binary, today)?;
            self.file = file;
            self.date = today;
            self.prune()?;
        }
        self.file.write_all(bytes)?;
        // Background stderr is an early-failure fallback; duplicating every
        // record there would grow the controller's launch-day console forever.
        if io::stderr().is_terminal() {
            io::stderr().write_all(bytes)?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Published only after both listeners bind. Normal exits remove it; supervisors
/// must also remove stale files after signals or crashes and validate the PID.
pub struct ReadyFile(PathBuf);
impl ReadyFile {
    pub fn create(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        writeln!(file, "{}", std::process::id())?;
        Ok(Self(path.to_path_buf()))
    }
}
impl Drop for ReadyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_logs_rotate_and_prune_only_owned_dated_files() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-runs/app-log-unit")
            .join(format!(
                "{}-{}",
                std::process::id(),
                chrono::Utc::now().timestamp_nanos_opt().unwrap()
            ));
        std::fs::create_dir_all(&dir).unwrap();
        let old = chrono::Local::now().date_naive() - chrono::Duration::days(30);
        let expired = dir.join(format!("exchange-{old}.log"));
        let unrelated = dir.join(format!("client-{old}.log"));
        std::fs::write(&expired, "old").unwrap();
        std::fs::write(&unrelated, "keep").unwrap();
        let mut writer = DailyWriter::new(dir.clone(), "exchange", 14).unwrap();
        assert!(!expired.exists());
        assert!(unrelated.exists());
        // Simulate the writer's previous day; the write must reopen today's file.
        writer.date = old;
        writer.write_all(b"rotation check\n").unwrap();
        writer.flush().unwrap();
        let today = chrono::Local::now().date_naive();
        assert_eq!(writer.date, today);
        assert!(
            std::fs::read_to_string(dir.join(format!("exchange-{today}.log")))
                .unwrap()
                .contains("rotation check")
        );
    }
}
