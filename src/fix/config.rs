//! parser for quickfix-style settings files ([DEFAULT]/[SESSION] sections with
//! key=value lines). Only the handful of keys this exchange needs are read.

use std::collections::HashMap;
use std::io::BufRead;

#[derive(Debug, Default, Clone)]
pub struct FixConfig {
    pub settings: HashMap<String, String>,
}

impl FixConfig {
    /// flatten all sections into one map; first occurrence of a key wins
    /// (session-specific keys come after [DEFAULT] in our configs)
    pub fn load<R: BufRead>(reader: R) -> std::io::Result<FixConfig> {
        let mut settings = HashMap::new();
        for line in reader.lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }
            if line.starts_with('[') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                settings
                    .entry(key.trim().to_string())
                    .or_insert_with(|| value.trim().to_string());
            }
        }
        Ok(FixConfig { settings })
    }

    pub fn load_file(path: &str) -> std::io::Result<FixConfig> {
        let file = std::fs::File::open(path)?;
        Self::load(std::io::BufReader::new(file))
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.settings.get(key).map(|s| s.as_str())
    }

    pub fn get_or(&self, key: &str, default: &str) -> String {
        self.get(key).unwrap_or(default).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_acceptor_config() {
        let data = "# comment\n[DEFAULT]\nSenderCompID=GOX\nSocketAcceptPort=5001\nConnectionType=acceptor\nBeginString=FIX.4.4\nDynamicSessions=Y\n";
        let cfg = FixConfig::load(data.as_bytes()).unwrap();
        assert_eq!(cfg.get("SenderCompID"), Some("GOX"));
        assert_eq!(cfg.get("SocketAcceptPort"), Some("5001"));
        assert_eq!(cfg.get("BeginString"), Some("FIX.4.4"));
    }

    #[test]
    fn test_parse_initiator_config() {
        let data = "[DEFAULT]\nTargetCompID=GOX\nSocketConnectPort=5001\nSocketConnectHost=localhost\nHeartBtInt=30\n";
        let cfg = FixConfig::load(data.as_bytes()).unwrap();
        assert_eq!(cfg.get_or("HeartBtInt", "30"), "30");
        assert_eq!(cfg.get("SocketConnectHost"), Some("localhost"));
        assert_eq!(cfg.get_or("Missing", "fallback"), "fallback");
    }
}
