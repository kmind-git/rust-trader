use std::sync::{Arc, Mutex};

use gotrader::core::exchange::Engine;
use gotrader::core::orderbook::Book;
use gotrader::fix::config::FixConfig;
use gotrader::fix::session::{run_acceptor, AcceptorConfig};
use gotrader::rest;

fn main() {
    let mut fix_path = "configs/qf_got_settings".to_string();
    let mut instruments_path = "configs/instruments.txt".to_string();
    let mut port = "8080".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-fix" => fix_path = args.next().unwrap_or_else(|| fix_path.clone()),
            "-instruments" => instruments_path = args.next().unwrap_or_else(|| instruments_path.clone()),
            "-port" => port = args.next().unwrap_or_else(|| port.clone()),
            other => println!("unknown argument {}", other),
        }
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let engine = Arc::new(Mutex::new(Engine::new()));
    if let Err(e) = engine.lock().unwrap().load_instruments(&instruments_path) {
        println!("unable to load instruments: {}", e);
    }

    let fix_config = match FixConfig::load_file(&fix_path) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("unable to load fix settings {}: {}", fix_path, e);
            std::process::exit(1);
        }
    };
    let acceptor_cfg = AcceptorConfig {
        port: fix_config.get_or("SocketAcceptPort", "5001").parse().unwrap_or(5001),
        sender_comp_id: fix_config.get_or("SenderCompID", "GOX"),
        begin_string: fix_config.get_or("BeginString", "FIX.4.2"),
    };

    // read-only REST api
    if let Err(e) = rest::start(Arc::clone(&engine), &format!("0.0.0.0:{}", port)) {
        eprintln!("unable to start web server: {}", e);
        std::process::exit(1);
    }
    println!("web server access available at :{}", port);

    // FIX acceptor
    {
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || {
            if let Err(e) = run_acceptor(engine, acceptor_cfg) {
                eprintln!("FIX acceptor failed: {}", e);
                std::process::exit(1);
            }
        });
    }

    // interactive console
    println!("use 'help' to get a list of commands");
    loop {
        print!("Command?");
        use std::io::Write;
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.first().copied() {
            None => continue,
            Some("help") => println!("The available commands are: quit, sessions, book SYMBOL, list"),
            Some("quit") => break,
            Some("sessions") => {
                println!("Active sessions: {}", engine.lock().unwrap().session_ids().join(","));
            }
            Some("book") => match parts.get(1) {
                Some(symbol) => match engine.lock().unwrap().book(symbol) {
                    Some(book) => println!("{}", format_book(symbol, &book)),
                    None => println!("no book for {}", symbol),
                },
                None => println!("usage: book SYMBOL"),
            },
            Some("list") => {
                for symbol in engine.lock().unwrap().all_symbols() {
                    println!("{}", symbol);
                }
            }
            Some(other) => println!("Unknown command, '{}' use 'help'", other),
        }
    }
}

fn format_book(symbol: &str, book: &Book) -> String {
    let levels = |levels: &[gotrader::core::orderbook::BookLevel]| -> String {
        levels
            .iter()
            .map(|l| format!("{} @ {}", l.quantity, l.price))
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        "book:{} bids: {} asks: {}",
        symbol,
        levels(&book.bids),
        levels(&book.asks)
    )
}
