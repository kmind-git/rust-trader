//! QuickFIX-compatible session settings.
//!
//! The settings format follows the part of QuickFIX's settings grammar that
//! this application consumes: one `[DEFAULT]` section and zero or more
//! `[SESSION]` sections. Values in a session override values from `[DEFAULT]`;
//! values are never flattened across sessions. Parsing validates values up
//! front so a process cannot silently run with a fallback after a typo.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::BufRead;

/// Keys understood by this implementation. `Logging`, `DynamicSessions` and
/// `TargetCompIDs` are project extensions documented in README/ADR-0005/6.
const KNOWN_KEYS: &[&str] = &[
    "ConnectionType",
    "BeginString",
    "SenderCompID",
    "TargetCompID",
    "SocketAcceptPort",
    "SocketConnectHost",
    "SocketConnectPort",
    "HeartBtInt",
    "ResetOnLogout",
    "ResetOnDisconnect",
    "PersistMessages",
    "UseDataDictionary",
    "DataDictionary",
    "FileLogPath",
    "Logging",
    "DynamicSessions",
    "TargetCompIDs",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Io(String),
    Invalid {
        line: usize,
        message: String,
    },
    UnknownKey {
        line: usize,
        key: String,
    },
    DuplicateKey {
        line: usize,
        key: String,
        section: String,
    },
    Missing {
        section: String,
        key: String,
    },
    Unsupported {
        key: String,
        value: String,
        message: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(message) => write!(f, "unable to read FIX settings: {message}"),
            ConfigError::Invalid { line, message } => {
                write!(f, "invalid FIX settings at line {line}: {message}")
            }
            ConfigError::UnknownKey { line, key } => {
                write!(f, "unknown FIX setting {key:?} at line {line}")
            }
            ConfigError::DuplicateKey { line, key, section } => {
                write!(f, "duplicate setting {key:?} in {section} at line {line}")
            }
            ConfigError::Missing { section, key } => {
                write!(f, "missing required setting {key:?} in {section}")
            }
            ConfigError::Unsupported {
                key,
                value,
                message,
            } => {
                write!(f, "unsupported value {value:?} for {key}: {message}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(value: std::io::Error) -> Self {
        ConfigError::Io(value.to_string())
    }
}

/// A resolved QuickFIX settings map. `FixConfig::session` returns this type
/// after applying DEFAULT inheritance and SESSION overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSettings {
    pub settings: HashMap<String, String>,
}

impl SessionSettings {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.settings.get(key).map(String::as_str)
    }

    pub fn get_or(&self, key: &str, default: &str) -> String {
        self.get(key).unwrap_or(default).to_string()
    }

    pub fn required(&self, section: &str, key: &str) -> Result<&str, ConfigError> {
        self.get(key).ok_or_else(|| ConfigError::Missing {
            section: section.to_string(),
            key: key.to_string(),
        })
    }

    /// Parse a QuickFIX boolean. QuickFIX settings use uppercase Y/N; other
    /// spellings are rejected rather than treated as false.
    pub fn bool(&self, key: &str, default: bool) -> Result<bool, ConfigError> {
        match self.get(key) {
            None => Ok(default),
            Some("Y") => Ok(true),
            Some("N") => Ok(false),
            Some(value) => Err(ConfigError::Unsupported {
                key: key.to_string(),
                value: value.to_string(),
                message: "expected Y or N".to_string(),
            }),
        }
    }

    pub fn required_bool(&self, section: &str, key: &str) -> Result<bool, ConfigError> {
        match self.get(key) {
            None => Err(ConfigError::Missing {
                section: section.to_string(),
                key: key.to_string(),
            }),
            Some("Y") => Ok(true),
            Some("N") => Ok(false),
            Some(value) => Err(ConfigError::Unsupported {
                key: key.to_string(),
                value: value.to_string(),
                message: "expected Y or N".to_string(),
            }),
        }
    }

    pub fn u16(&self, key: &str, default: u16) -> Result<u16, ConfigError> {
        match self.get(key) {
            None => Ok(default),
            Some(value) => parse_u16(key, value),
        }
    }

    pub fn required_u16(&self, section: &str, key: &str) -> Result<u16, ConfigError> {
        let value = self.required(section, key)?;
        parse_u16(key, value)
    }

    pub fn u32(&self, key: &str, default: u32) -> Result<u32, ConfigError> {
        match self.get(key) {
            None => Ok(default),
            Some(value) => parse_u32(key, value),
        }
    }

    pub fn required_u32(&self, section: &str, key: &str) -> Result<u32, ConfigError> {
        let value = self.required(section, key)?;
        parse_u32(key, value)
    }
}

fn parse_u16(key: &str, value: &str) -> Result<u16, ConfigError> {
    let parsed = value.parse::<u16>().map_err(|_| ConfigError::Unsupported {
        key: key.to_string(),
        value: value.to_string(),
        message: "expected an unsigned 16-bit integer".to_string(),
    })?;
    if parsed == 0 {
        return Err(ConfigError::Unsupported {
            key: key.to_string(),
            value: value.to_string(),
            message: "port must be between 1 and 65535".to_string(),
        });
    }
    Ok(parsed)
}

fn parse_u32(key: &str, value: &str) -> Result<u32, ConfigError> {
    let parsed = value.parse::<u32>().map_err(|_| ConfigError::Unsupported {
        key: key.to_string(),
        value: value.to_string(),
        message: "expected an unsigned 32-bit integer".to_string(),
    })?;
    if key == "HeartBtInt" && parsed == 0 {
        return Err(ConfigError::Unsupported {
            key: key.to_string(),
            value: value.to_string(),
            message: "must be greater than zero".to_string(),
        });
    }
    Ok(parsed)
}

/// Parsed settings file. `defaults` contains only `[DEFAULT]` values and
/// `sessions` contains only the values written in each `[SESSION]` block.
/// Use [`FixConfig::session`] for a resolved map.
#[derive(Debug, Clone, Default)]
pub struct FixConfig {
    pub defaults: SessionSettings,
    pub sessions: Vec<SessionSettings>,
}

impl FixConfig {
    pub fn load<R: BufRead>(reader: R) -> Result<FixConfig, ConfigError> {
        let mut defaults = HashMap::new();
        let mut sessions: Vec<HashMap<String, String>> = Vec::new();
        let mut section = Section::None;

        for (line_index, raw_line) in reader.lines().enumerate() {
            let line_number = line_index + 1;
            let line = raw_line?.trim().to_string();
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }
            if line.starts_with('[') {
                if !line.ends_with(']') {
                    return Err(ConfigError::Invalid {
                        line: line_number,
                        message: "section header must end with ]".to_string(),
                    });
                }
                section = match line.as_str() {
                    "[DEFAULT]" => Section::Default,
                    "[SESSION]" => {
                        sessions.push(HashMap::new());
                        Section::Session(sessions.len() - 1)
                    }
                    _ => {
                        return Err(ConfigError::Invalid {
                            line: line_number,
                            message: format!(
                                "unsupported section {line:?}; expected [DEFAULT] or [SESSION]"
                            ),
                        })
                    }
                };
                continue;
            }

            let (key, value) = line.split_once('=').ok_or_else(|| ConfigError::Invalid {
                line: line_number,
                message: "setting must use key=value syntax".to_string(),
            })?;
            let key = key.trim();
            let value = value.trim();
            if key.is_empty() {
                return Err(ConfigError::Invalid {
                    line: line_number,
                    message: "setting key must not be empty".to_string(),
                });
            }
            if value.is_empty() {
                return Err(ConfigError::Invalid {
                    line: line_number,
                    message: format!("setting {key:?} must not be empty"),
                });
            }
            if !KNOWN_KEYS.contains(&key) {
                return Err(ConfigError::UnknownKey {
                    line: line_number,
                    key: key.to_string(),
                });
            }

            let target = match section {
                Section::None => {
                    return Err(ConfigError::Invalid {
                        line: line_number,
                        message: "setting appears before [DEFAULT] or [SESSION]".to_string(),
                    })
                }
                Section::Default => &mut defaults,
                Section::Session(index) => &mut sessions[index],
            };
            if target.contains_key(key) {
                return Err(ConfigError::DuplicateKey {
                    line: line_number,
                    key: key.to_string(),
                    section: section.name().to_string(),
                });
            }
            target.insert(key.to_string(), value.to_string());
        }

        let config = FixConfig {
            defaults: SessionSettings { settings: defaults },
            sessions: sessions
                .into_iter()
                .map(|settings| SessionSettings { settings })
                .collect(),
        };
        config.validate_values()?;
        Ok(config)
    }

    pub fn load_file(path: &str) -> Result<FixConfig, ConfigError> {
        let file = std::fs::File::open(path)?;
        Self::load(std::io::BufReader::new(file))
    }

    /// Resolve one `[SESSION]` block by inheriting all DEFAULT values and
    /// applying its session-specific overrides.
    pub fn session(&self, index: usize) -> Option<SessionSettings> {
        self.sessions.get(index).map(|session| {
            let mut settings = self.defaults.settings.clone();
            settings.extend(
                session
                    .settings
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
            SessionSettings { settings }
        })
    }

    pub fn resolved_sessions(&self) -> Vec<SessionSettings> {
        (0..self.sessions.len())
            .filter_map(|index| self.session(index))
            .collect()
    }

    /// Resolve the single acceptor session. Multiple acceptors are an
    /// explicit configuration error so a binary cannot pick one silently.
    pub fn acceptor(&self) -> Result<SessionSettings, ConfigError> {
        self.unique_role("acceptor")
    }

    /// Resolve all declared acceptor sessions in file order. A listener may
    /// serve several QuickFIX sessions on one socket; callers should use the
    /// returned identities for admission and reject incompatible ports or
    /// protocol versions before starting it.
    pub fn acceptors(&self) -> Vec<SessionSettings> {
        self.resolved_sessions()
            .into_iter()
            .filter(|settings| settings.get("ConnectionType") == Some("acceptor"))
            .collect()
    }

    /// Resolve one initiator session. `sender_comp_id` selects among multiple
    /// configured initiators when the command line supplies `-id`.
    pub fn initiator(&self, sender_comp_id: Option<&str>) -> Result<SessionSettings, ConfigError> {
        let mut selected: Vec<SessionSettings> = self
            .resolved_sessions()
            .into_iter()
            .filter(|settings| settings.get("ConnectionType") == Some("initiator"))
            .collect();
        if let Some(sender) = sender_comp_id {
            selected.retain(|settings| settings.get("SenderCompID") == Some(sender));
        }
        match selected.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(ConfigError::Missing {
                section: "[SESSION]".to_string(),
                key: "initiator session".to_string(),
            }),
            _ => Err(ConfigError::Invalid {
                line: 0,
                message: "multiple initiator sessions match; specify -id".to_string(),
            }),
        }
    }

    pub fn initiators(&self) -> Vec<SessionSettings> {
        self.resolved_sessions()
            .into_iter()
            .filter(|settings| settings.get("ConnectionType") == Some("initiator"))
            .collect()
    }

    /// Validate the implementation profile consumed by the current
    /// in-memory session engine. QuickFIX accepts these switches in general,
    /// but this application deliberately has no message store or sequence
    /// recovery implementation yet. Startup must fail loudly instead of
    /// claiming that those settings took effect.
    pub fn validate_supported_runtime(settings: &SessionSettings) -> Result<(), ConfigError> {
        if settings.bool("PersistMessages", true)? {
            return Err(ConfigError::Unsupported {
                key: "PersistMessages".to_string(),
                value: "Y".to_string(),
                message: "message persistence is not implemented; use PersistMessages=N"
                    .to_string(),
            });
        }
        if !settings.bool("ResetOnDisconnect", false)? {
            return Err(ConfigError::Unsupported {
                key: "ResetOnDisconnect".to_string(),
                value: "N".to_string(),
                message:
                    "non-resetting disconnect recovery is not implemented; use ResetOnDisconnect=Y"
                        .to_string(),
            });
        }
        if !settings.bool("ResetOnLogout", false)? {
            return Err(ConfigError::Unsupported {
                key: "ResetOnLogout".to_string(),
                value: "N".to_string(),
                message: "non-resetting logout recovery is not implemented; use ResetOnLogout=Y"
                    .to_string(),
            });
        }
        if !settings.bool("UseDataDictionary", true)? {
            return Err(ConfigError::Unsupported {
                key: "UseDataDictionary".to_string(),
                value: "N".to_string(),
                message: "the built-in FIX.4.2 dictionary is required; use UseDataDictionary=Y"
                    .to_string(),
            });
        }
        if let Some(path) = settings.get("DataDictionary") {
            if path != "FIX42.xml" && path != "builtin" {
                return Err(ConfigError::Unsupported {
                    key: "DataDictionary".to_string(),
                    value: path.to_string(),
                    message: "only the built-in FIX.4.2 profile is supported".to_string(),
                });
            }
        }
        Ok(())
    }

    /// Compatibility accessor for callers that only need DEFAULT values.
    /// New code should resolve a session first.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.defaults.get(key)
    }

    pub fn get_or(&self, key: &str, default: &str) -> String {
        self.get(key).unwrap_or(default).to_string()
    }

    fn unique_role(&self, role: &str) -> Result<SessionSettings, ConfigError> {
        let selected: Vec<_> = self
            .resolved_sessions()
            .into_iter()
            .filter(|settings| settings.get("ConnectionType") == Some(role))
            .collect();
        match selected.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(ConfigError::Missing {
                section: "[SESSION]".to_string(),
                key: format!("{role} session"),
            }),
            _ => Err(ConfigError::Invalid {
                line: 0,
                message: format!("multiple {role} sessions are not supported by this binary"),
            }),
        }
    }

    fn validate_values(&self) -> Result<(), ConfigError> {
        let mut resolved = vec![self.defaults.clone()];
        resolved.extend(self.resolved_sessions());
        for settings in resolved {
            if let Some(value) = settings.get("ConnectionType") {
                if value != "acceptor" && value != "initiator" {
                    return Err(ConfigError::Unsupported {
                        key: "ConnectionType".to_string(),
                        value: value.to_string(),
                        message: "expected acceptor or initiator".to_string(),
                    });
                }
            }
            if let Some(value) = settings.get("BeginString") {
                if value != "FIX.4.2" {
                    return Err(ConfigError::Unsupported {
                        key: "BeginString".to_string(),
                        value: value.to_string(),
                        message: "only the FIX.4.2 profile is implemented".to_string(),
                    });
                }
            }
            for key in [
                "ResetOnLogout",
                "ResetOnDisconnect",
                "PersistMessages",
                "UseDataDictionary",
                "Logging",
                "DynamicSessions",
            ] {
                if settings.get(key).is_some() {
                    settings.bool(key, false)?;
                }
            }
            if settings.get("HeartBtInt").is_some() {
                settings.u32("HeartBtInt", 1)?;
            }
            for key in ["SocketAcceptPort", "SocketConnectPort"] {
                if settings.get(key).is_some() {
                    settings.u16(key, 1)?;
                }
            }
            for key in ["SenderCompID", "TargetCompID"] {
                if let Some(value) = settings.get(key) {
                    validate_comp_id(key, value)?;
                }
            }
            if let Some(value) = settings.get("TargetCompIDs") {
                for target in value.split(',') {
                    let target = target.trim();
                    if target.is_empty() {
                        return Err(ConfigError::Unsupported {
                            key: "TargetCompIDs".to_string(),
                            value: value.to_string(),
                            message: "comma-separated IDs must not contain empty entries"
                                .to_string(),
                        });
                    }
                    validate_comp_id("TargetCompIDs", target)?;
                }
            }
        }

        // A settings file with sessions must identify each session. This
        // catches a typo before a process starts.
        let mut identities = HashSet::new();
        for (index, settings) in self.resolved_sessions().iter().enumerate() {
            let section = format!("[SESSION] #{index}");
            settings.required(&section, "ConnectionType")?;
            settings.required(&section, "BeginString")?;
            if let (Some(begin), Some(sender), Some(target)) = (
                settings.get("BeginString"),
                settings.get("SenderCompID"),
                settings.get("TargetCompID"),
            ) {
                let identity = format!("{begin}|{sender}|{target}");
                if !identities.insert(identity) {
                    return Err(ConfigError::Invalid {
                        line: 0,
                        message: format!("duplicate QuickFIX session identity in {section}"),
                    });
                }
            }
        }
        Ok(())
    }
}

fn validate_comp_id(key: &str, value: &str) -> Result<(), ConfigError> {
    if value.is_empty()
        || value.len() > 64
        || !value.is_ascii()
        || value
            .chars()
            .any(|c| c.is_ascii_control() || c.is_whitespace())
    {
        return Err(ConfigError::Unsupported {
            key: key.to_string(),
            value: value.to_string(),
            message: "CompID must be 1-64 ASCII non-whitespace printable characters".to_string(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum Section {
    None,
    Default,
    Session(usize),
}

impl Section {
    fn name(self) -> &'static str {
        match self {
            Section::None => "<none>",
            Section::Default => "[DEFAULT]",
            Section::Session(_) => "[SESSION]",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values_are_inherited_and_session_values_override() {
        let data = "[DEFAULT]\nConnectionType=initiator\nBeginString=FIX.4.2\nSocketConnectHost=localhost\nSocketConnectPort=5001\nHeartBtInt=30\nLogging=Y\n[SESSION]\nSenderCompID=CLIENT\nTargetCompID=GOX\n[SESSION]\nSenderCompID=PLAYBACK\nTargetCompID=GOX\nHeartBtInt=5\n";
        let cfg = FixConfig::load(data.as_bytes()).unwrap();
        assert_eq!(cfg.sessions.len(), 2);
        let first = cfg.session(0).unwrap();
        let second = cfg.session(1).unwrap();
        assert_eq!(first.get("HeartBtInt"), Some("30"));
        assert_eq!(second.get("HeartBtInt"), Some("5"));
        assert_eq!(first.get("BeginString"), Some("FIX.4.2"));
    }

    #[test]
    fn parser_rejects_unknown_keys_and_invalid_values() {
        let unknown = "[DEFAULT]\nNotAQuickFixKey=Y\n";
        assert!(matches!(
            FixConfig::load(unknown.as_bytes()),
            Err(ConfigError::UnknownKey { .. })
        ));

        let bad_bool = "[DEFAULT]\nLogging=true\n";
        assert!(
            matches!(FixConfig::load(bad_bool.as_bytes()), Err(ConfigError::Unsupported { key, .. }) if key == "Logging")
        );

        let bad_port = "[DEFAULT]\nSocketAcceptPort=nope\n";
        assert!(
            matches!(FixConfig::load(bad_port.as_bytes()), Err(ConfigError::Unsupported { key, .. }) if key == "SocketAcceptPort")
        );
    }

    #[test]
    fn parser_rejects_non_fix42_profile_and_missing_section() {
        let wrong_version = "[SESSION]\nConnectionType=initiator\nBeginString=FIX.4.4\n";
        assert!(
            matches!(FixConfig::load(wrong_version.as_bytes()), Err(ConfigError::Unsupported { key, .. }) if key == "BeginString")
        );
        let missing = "[SESSION]\nBeginString=FIX.4.2\n";
        assert!(
            matches!(FixConfig::load(missing.as_bytes()), Err(ConfigError::Missing { key, .. }) if key == "ConnectionType")
        );
        let non_ascii =
            "[SESSION]\nConnectionType=initiator\nBeginString=FIX.4.2\nSenderCompID=客户\n";
        assert!(
            matches!(FixConfig::load(non_ascii.as_bytes()), Err(ConfigError::Unsupported { key, .. }) if key == "SenderCompID")
        );
    }

    #[test]
    fn role_selection_requires_a_single_matching_session() {
        let data = "[DEFAULT]\nBeginString=FIX.4.2\n[SESSION]\nConnectionType=acceptor\nSenderCompID=GOX\nSocketAcceptPort=5001\n";
        let cfg = FixConfig::load(data.as_bytes()).unwrap();
        assert_eq!(cfg.acceptor().unwrap().get("SenderCompID"), Some("GOX"));
        assert!(cfg.initiator(None).is_err());
    }

    #[test]
    fn strict_helpers_parse_supported_values() {
        let data = "[DEFAULT]\nLogging=N\nSocketAcceptPort=5001\nHeartBtInt=30\n";
        let cfg = FixConfig::load(data.as_bytes()).unwrap();
        assert!(!cfg.defaults.bool("Logging", true).unwrap());
        assert_eq!(cfg.defaults.u16("SocketAcceptPort", 1).unwrap(), 5001);
        assert_eq!(cfg.defaults.u32("HeartBtInt", 1).unwrap(), 30);
    }

    #[test]
    fn unsupported_runtime_switches_fail_explicitly() {
        let data = "[DEFAULT]\nPersistMessages=Y\n";
        let cfg = FixConfig::load(data.as_bytes()).unwrap();
        assert!(
            matches!(FixConfig::validate_supported_runtime(&cfg.defaults), Err(ConfigError::Unsupported { key, .. }) if key == "PersistMessages")
        );

        let data = "[DEFAULT]\nPersistMessages=N\nResetOnDisconnect=N\n";
        let cfg = FixConfig::load(data.as_bytes()).unwrap();
        assert!(
            matches!(FixConfig::validate_supported_runtime(&cfg.defaults), Err(ConfigError::Unsupported { key, .. }) if key == "ResetOnDisconnect")
        );
    }

    #[test]
    fn checked_in_quickfix_samples_resolve_their_sessions() {
        let acceptor =
            FixConfig::load(include_str!("../../configs/qf_exchange_settings").as_bytes()).unwrap();
        // CLIENT, PLAYBACK and ORDERHUB are declared in the checked-in sample
        assert_eq!(acceptor.acceptors().len(), 3);
        assert_eq!(
            acceptor.acceptors()[2].get("TargetCompID"),
            Some("ORDERHUB")
        );
        assert_eq!(
            acceptor.acceptors()[0].get("SocketAcceptPort"),
            Some("5001")
        );
        let initiator =
            FixConfig::load(include_str!("../../configs/qf_connector_settings").as_bytes())
                .unwrap();
        assert_eq!(initiator.initiators().len(), 2);
        assert_eq!(
            initiator
                .initiator(Some("PLAYBACK"))
                .unwrap()
                .get("TargetCompID"),
            Some("GOX")
        );
    }

    #[test]
    fn duplicate_session_identity_is_rejected() {
        let data = "[DEFAULT]\nConnectionType=initiator\nBeginString=FIX.4.2\n[SESSION]\nSenderCompID=CLIENT\nTargetCompID=GOX\n[SESSION]\nSenderCompID=CLIENT\nTargetCompID=GOX\n";
        assert!(matches!(
            FixConfig::load(data.as_bytes()),
            Err(ConfigError::Invalid { .. })
        ));
    }
}
