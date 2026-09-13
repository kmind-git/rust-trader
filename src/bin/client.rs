use std::io::BufRead;

use gotrader::core::instrument::Instrument;
use gotrader::core::order::Side;
use gotrader::fix::config::FixConfig;
use gotrader::fix::log::LogConfig;
use gotrader::fix::session::{Callback, FillView, Initiator, InitiatorConfig, OrderView};

struct PrintCallback;

impl Callback for PrintCallback {
    fn on_instrument(&mut self, instrument: &Instrument) {
        println!("instrument {} id {}", instrument.symbol, instrument.id);
    }

    fn on_order_status(&mut self, order: &OrderView) {
        let state = if order.state.is_active() { "active" } else { "inactive" };
        println!(
            "order {} {} {} {:?} qty {} @ {} remaining {} ({})",
            order.id, order.symbol, order.side.as_str(), order.state,
            d(order.quantity), d(order.price), d(order.remaining), state
        );
    }

    fn on_fill(&mut self, fill: &FillView) {
        println!(
            "fill {} {} {} @ {}",
            fill.symbol, fill.side.as_str(), d(fill.quantity), d(fill.price)
        );
    }
}

/// strip trailing zeros for display ("3.0000" -> "3")
fn d(v: rust_decimal::Decimal) -> String {
    v.normalize().to_string()
}

fn main() {
    let mut fix_path = "configs/qf_connector_settings".to_string();
    let mut sender_comp_id = "CLIENT".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-fix" => fix_path = args.next().unwrap_or_else(|| fix_path.clone()),
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
        log: LogConfig::from_config(&config, "logs/client"),
    };

    let mut initiator =
        Initiator::connect(initiator_cfg, Box::new(PrintCallback)).expect("exchange is not connected");
    println!("commands: buy|sell SYMBOL QTY [PRICE] | modify ID PRICE QTY | cancel ID | quit");
    let stdin = std::io::stdin();
    loop {
        print!("Command?");
        use std::io::Write;
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.first().copied() {
            None => continue,
            Some("quit") => break,
            Some(cmd @ ("buy" | "sell")) => {
                if parts.len() != 3 && parts.len() != 4 {
                    println!("usage: buy|sell SYMBOL QTY [PRICE]");
                    continue;
                }
                let symbol = parts[1];
                let side = if cmd == "buy" { Side::Buy } else { Side::Sell };
                let quantity = match parts[2].parse::<rust_decimal::Decimal>() {
                    Ok(q) => q,
                    Err(_) => {
                        println!("invalid quantity {}", parts[2]);
                        continue;
                    }
                };
                let result = if parts.len() == 4 {
                    match parts[3].parse::<rust_decimal::Decimal>() {
                        Ok(price) => initiator.create_order(symbol, side, gotrader::core::order::OrderType::Limit, price, quantity),
                        Err(_) => {
                            println!("invalid price {}", parts[3]);
                            continue;
                        }
                    }
                } else {
                    initiator.create_order(symbol, side, gotrader::core::order::OrderType::Market, rust_decimal::Decimal::ZERO, quantity)
                };
                if let Err(_) = result {
                    println!("unable to submit order: not connected");
                }
            }
            Some("cancel") => match parts.len() {
                2 => match parts[1].parse::<i32>() {
                    Ok(id) => {
                        if initiator.cancel_order(id).is_err() {
                            println!("unable to cancel order {} (unknown id or not connected)", id);
                        }
                    }
                    Err(_) => println!("invalid order id {}", parts[1]),
                },
                _ => println!("usage: cancel ID"),
            },
            Some("modify") => match (parts.len(), parts[1].parse::<i32>()) {
                (4, Ok(id)) => match (parts[2].parse::<rust_decimal::Decimal>(), parts[3].parse::<rust_decimal::Decimal>()) {
                    (Ok(price), Ok(quantity)) => {
                        if initiator.modify_order(id, price, quantity).is_err() {
                            println!("unable to modify order {} (unknown id or not connected)", id);
                        }
                    }
                    _ => println!("invalid price or quantity"),
                },
                _ => println!("usage: modify ID PRICE QTY"),
            },
            Some(other) => println!("unknown command '{}', use buy|sell SYMBOL QTY [PRICE] | modify ID PRICE QTY | cancel ID | quit", other),
        }
    }
    initiator.disconnect();
    // give the reader thread a moment to deliver reports already in flight
    std::thread::sleep(std::time::Duration::from_millis(300));
    println!("we are logged out!");
}
