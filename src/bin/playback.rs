use std::io::BufRead;

use gotrader::fix::config::FixConfig;
use gotrader::fix::session::{Callback, Initiator, InitiatorConfig};

struct NopCallback;

impl Callback for NopCallback {}

/// parse a playback timestamp: "+5s"/"+100ms"/"+2min" style relative offsets, or
/// absolute epoch milliseconds (diffed against the previous line).
/// Mirrors calcDuration in the Go implementation.
fn calc_duration(last_timestamp: Option<&str>, timestamp: &str) -> Result<std::time::Duration, String> {
    if let Some(rest) = timestamp.strip_prefix('+') {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let suffix = &rest[digits.len()..];
        let n: u64 = digits.parse().map_err(|e| format!("bad relative timestamp {:?}: {}", timestamp, e))?;
        let unit = match suffix {
            "us" => 1_000u64,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "min" => 60_000_000_000,
            _ => return Err(format!("unknown timestamp suffix {:?}", suffix)),
        };
        return Ok(std::time::Duration::from_nanos(n * unit));
    }
    // absolute milliseconds: diff against the previous line
    let last = last_timestamp.ok_or("previous timestamp must be relative to use absolute timestamps")?;
    if last.starts_with('+') {
        return Err("previous timestamp must be absolute to use absolute timestamps".to_string());
    }
    let last_ms: u64 = last.parse().map_err(|e| format!("bad timestamp {:?}: {}", last, e))?;
    let ms: u64 = timestamp.parse().map_err(|e| format!("bad timestamp {:?}: {}", timestamp, e))?;
    Ok(std::time::Duration::from_millis(ms.saturating_sub(last_ms)))
}

fn main() {
    let mut fix_path = "configs/qf_connector_settings".to_string();
    let mut file_path = "configs/playback.txt".to_string();
    let mut speed = 1.0f64;
    let mut sender_comp_id = "PLAYBACK".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-fix" => fix_path = args.next().unwrap_or_else(|| fix_path.clone()),
            "-file" => file_path = args.next().unwrap_or_else(|| file_path.clone()),
            "-speed" => speed = args.next().and_then(|v| v.parse().ok()).unwrap_or(1.0),
            "-id" => sender_comp_id = args.next().unwrap_or_else(|| sender_comp_id.clone()),
            other => println!("unknown argument {}", other),
        }
    }

    let config = match FixConfig::load_file(&fix_path) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("unable to load fix settings {}: {}", fix_path, e);
            std::process::exit(1);
        }
    };
    let initiator_cfg = InitiatorConfig {
        sender_comp_id,
        target_comp_id: config.get_or("TargetCompID", "GOX"),
        host: config.get_or("SocketConnectHost", "localhost"),
        port: config.get_or("SocketConnectPort", "5001").parse().unwrap_or(5001),
        heart_bt_int: config.get_or("HeartBtInt", "30").parse().unwrap_or(30),
    };

    let mut initiator = Initiator::connect(initiator_cfg, Box::new(NopCallback)).expect("exchange is not connected");

    let file = std::fs::File::open(&file_path).expect("unable to open playback file");
    let mut last_timestamp: Option<String> = None;
    for line in std::io::BufReader::new(file).lines() {
        let line = line.unwrap();
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
        let bid_qty: rust_decimal::Decimal = parts[2].parse().expect("bad qty");
        let bid_price: rust_decimal::Decimal = parts[3].parse().expect("bad price");
        let ask_qty: rust_decimal::Decimal = parts[4].parse().expect("bad qty");
        let ask_price: rust_decimal::Decimal = parts[5].parse().expect("bad price");

        initiator
            .quote(symbol, bid_price, bid_qty, ask_price, ask_qty)
            .expect("unable to submit quote");

        let duration = calc_duration(last_timestamp.as_deref(), timestamp).unwrap_or_default();
        if !duration.is_zero() {
            std::thread::sleep(duration.div_f64(speed));
        }
        last_timestamp = Some(timestamp.to_string());
    }
    initiator.disconnect();
}
