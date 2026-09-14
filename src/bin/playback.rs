use std::io::BufRead;

use gotrader::fix::config::{ConfigError, FixConfig, SessionSettings};
use gotrader::fix::log::LogConfig;
use gotrader::fix::session::{Callback, Initiator, InitiatorConfig};

struct NopCallback;

impl Callback for NopCallback {}

/// parse a playback timestamp: "+5s"/"+100ms"/"+2min" style relative offsets, or
/// absolute epoch milliseconds (diffed against the previous line).
/// Mirrors calcDuration in the Go implementation.
fn calc_duration(
    last_timestamp: Option<&str>,
    timestamp: &str,
) -> Result<std::time::Duration, String> {
    if let Some(rest) = timestamp.strip_prefix('+') {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let suffix = &rest[digits.len()..];
        let n: u64 = digits
            .parse()
            .map_err(|e| format!("bad relative timestamp {:?}: {}", timestamp, e))?;
        let unit = match suffix {
            "us" => 1_000u64,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "min" => 60_000_000_000,
            _ => return Err(format!("unknown timestamp suffix {:?}", suffix)),
        };
        return n
            .checked_mul(unit)
            .map(std::time::Duration::from_nanos)
            .ok_or_else(|| format!("relative timestamp {:?} is too large", timestamp));
    }
    // absolute milliseconds: diff against the previous line
    let last =
        last_timestamp.ok_or("previous timestamp must be relative to use absolute timestamps")?;
    if last.starts_with('+') {
        return Err("previous timestamp must be absolute to use absolute timestamps".to_string());
    }
    let last_ms: u64 = last
        .parse()
        .map_err(|e| format!("bad timestamp {:?}: {}", last, e))?;
    let ms: u64 = timestamp
        .parse()
        .map_err(|e| format!("bad timestamp {:?}: {}", timestamp, e))?;
    Ok(std::time::Duration::from_millis(ms.saturating_sub(last_ms)))
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut fix_path = "configs/qf_connector_settings".to_string();
    let mut file_path = "configs/playback.txt".to_string();
    let mut speed = 1.0f64;
    let mut sender_comp_id = "PLAYBACK".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-fix" => match args.next() {
                Some(value) if !value.is_empty() => fix_path = value,
                _ => die("-fix requires a settings file path"),
            },
            "-file" => match args.next() {
                Some(value) if !value.is_empty() => file_path = value,
                _ => die("-file requires a playback file path"),
            },
            "-speed" => {
                let value = args.next().unwrap_or_default();
                speed = match value.parse::<f64>() {
                    Ok(value) if value.is_finite() && value > 0.0 => value,
                    _ => die("-speed must be a finite number greater than zero"),
                };
            }
            "-id" => match args.next() {
                Some(value) if !value.is_empty() => sender_comp_id = value,
                _ => die("-id requires a SenderCompID"),
            },
            "-h" | "--help" => {
                println!("usage: playback [-fix SETTINGS] [-file PLAYBACK] [-speed POSITIVE] [-id SENDER_COMP_ID]");
                return;
            }
            other => die(&format!("unknown argument {other:?}")),
        }
    }

    let config = match FixConfig::load_file(&fix_path) {
        Ok(config) => config,
        Err(e) => die(&format!("unable to load fix settings {fix_path}: {e}")),
    };
    let settings = match config.initiator(Some(&sender_comp_id)) {
        Ok(settings) => settings,
        Err(e) => die_config(e),
    };
    if let Err(e) = FixConfig::validate_supported_runtime(&settings) {
        die_config(e);
    }
    let sender_comp_id = required(&settings, "SenderCompID");
    let target_comp_id = required(&settings, "TargetCompID");
    let host = required(&settings, "SocketConnectHost");
    let port = match settings.required_u16("initiator [SESSION]", "SocketConnectPort") {
        Ok(port) => port,
        Err(e) => die_config(e),
    };
    let heart_bt_int = match settings.required_u32("initiator [SESSION]", "HeartBtInt") {
        Ok(value) => value,
        Err(e) => die_config(e),
    };
    let log = match LogConfig::from_settings(&settings, "logs/playback") {
        Ok(log) => log,
        Err(e) => die_config(e),
    };
    let initiator_cfg = InitiatorConfig {
        sender_comp_id,
        target_comp_id,
        host,
        port,
        heart_bt_int,
        log,
    };

    let mut initiator = match Initiator::connect(initiator_cfg, Box::new(NopCallback)) {
        Ok(initiator) => initiator,
        Err(error) => die(&format!("unable to connect to exchange: {error}")),
    };

    let file = match std::fs::File::open(&file_path) {
        Ok(file) => file,
        Err(error) => die(&format!(
            "unable to open playback file {file_path}: {error}"
        )),
    };
    let mut last_timestamp: Option<String> = None;
    for line in std::io::BufReader::new(file).lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => die(&format!(
                "unable to read playback file {file_path}: {error}"
            )),
        };
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 6 {
            println!("invalid format {}", line);
            continue;
        }
        let timestamp = parts[0];
        let symbol = parts[1];
        let bid_qty = match parts[2].parse::<rust_decimal::Decimal>() {
            Ok(value) => value,
            Err(_) => die(&format!(
                "bad bid quantity {:?} in line {:?}",
                parts[2], line
            )),
        };
        let bid_price = match parts[3].parse::<rust_decimal::Decimal>() {
            Ok(value) => value,
            Err(_) => die(&format!("bad bid price {:?} in line {:?}", parts[3], line)),
        };
        let ask_qty = match parts[4].parse::<rust_decimal::Decimal>() {
            Ok(value) => value,
            Err(_) => die(&format!(
                "bad ask quantity {:?} in line {:?}",
                parts[4], line
            )),
        };
        let ask_price = match parts[5].parse::<rust_decimal::Decimal>() {
            Ok(value) => value,
            Err(_) => die(&format!("bad ask price {:?} in line {:?}", parts[5], line)),
        };

        initiator
            .quote(symbol, bid_price, bid_qty, ask_price, ask_qty)
            .expect("unable to submit quote");

        let duration = calc_duration(last_timestamp.as_deref(), timestamp).unwrap_or_default();
        if !duration.is_zero() {
            let seconds = duration.as_secs_f64() / speed;
            if !seconds.is_finite() || seconds > std::time::Duration::MAX.as_secs_f64() {
                die("scaled playback delay is too large for the configured -speed");
            }
            std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
        }
        last_timestamp = Some(timestamp.to_string());
    }
    initiator.disconnect();
}

fn required(settings: &SessionSettings, key: &str) -> String {
    settings
        .get(key)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            die_config(ConfigError::Missing {
                section: "initiator [SESSION]".to_string(),
                key: key.to_string(),
            })
        })
}

fn die_config(error: ConfigError) -> ! {
    die(&error.to_string())
}

fn die(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1);
}
