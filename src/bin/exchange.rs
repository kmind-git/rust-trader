use std::sync::{Arc, Mutex};

use rust_trader::core::exchange::{Engine, FillPolicy, MAX_TIER_FILLS};
use rust_trader::core::orderbook::Book;
use rust_trader::fix::config::{ConfigError, FixConfig, SessionSettings};
use rust_trader::fix::log::{LogConfig, SessionLog};
use rust_trader::fix::session::{bind_acceptor, run_acceptor_on, AcceptorConfig, Admission};
use rust_trader::rest;
use rust_trader::runtime::{self, ReadyFile, RuntimeSettings};

fn main() {
    let root = runtime::installation_root()
        .unwrap_or_else(|e| die(&format!("unable to locate installation: {e}")));
    let runtime_settings = RuntimeSettings::load(&root)
        .unwrap_or_else(|e| die(&format!("invalid runtime settings: {e}")));
    let mut fix_path = root
        .join("configs/qf_exchange_settings")
        .to_string_lossy()
        .into_owned();
    let mut instruments_path = root
        .join("configs/instruments.txt")
        .to_string_lossy()
        .into_owned();
    let mut port = runtime_settings.value("REST_PORT", "8080");
    let mut server_mode = false;
    let mut check_only = false;
    let mut ready_path = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-fix" => match args.next() {
                Some(value) if !value.is_empty() => fix_path = value,
                _ => die("-fix requires a settings file path"),
            },
            "-instruments" => match args.next() {
                Some(value) if !value.is_empty() => instruments_path = value,
                _ => die("-instruments requires an instruments file path"),
            },
            "-port" => match args.next() {
                Some(value) if !value.is_empty() => port = value,
                _ => die("-port requires a REST port"),
            },
            "--server" => server_mode = true,
            "--check" => check_only = true,
            "--ready-file" => match args.next() {
                Some(value) if !value.is_empty() => {
                    ready_path = Some(std::path::PathBuf::from(value))
                }
                _ => die("--ready-file requires a path"),
            },
            "-h" | "--help" => {
                println!("usage: exchange [-fix SETTINGS] [-instruments FILE] [-port REST_PORT] [--server] [--check] [--ready-file PATH]");
                return;
            }
            other => die(&format!("unknown argument {other:?}")),
        }
    }

    let rest_port = match port.parse::<u16>() {
        Ok(value) if value > 0 => value,
        _ => die("-port must be an integer between 1 and 65535"),
    };

    runtime_settings
        .init_logging("exchange")
        .unwrap_or_else(|e| die(&format!("unable to initialize application log: {e}")));

    let fix_config = match FixConfig::load_file(&fix_path) {
        Ok(config) => config,
        Err(e) => die(&format!("unable to load fix settings {fix_path}: {e}")),
    };
    let sessions = fix_config.acceptors();
    if sessions.is_empty() {
        die_config(ConfigError::Missing {
            section: "[SESSION]".to_string(),
            key: "acceptor session".to_string(),
        });
    }
    let first = &sessions[0];
    let begin_string = required(first, "BeginString", "acceptor [SESSION]");
    let sender_comp_id = required(first, "SenderCompID", "acceptor [SESSION]");
    let port = match first.required_u16("acceptor [SESSION]", "SocketAcceptPort") {
        Ok(value) => value,
        Err(error) => die_config(error),
    };
    if let Err(error) = FixConfig::validate_supported_runtime(first) {
        die_config(error);
    }
    // FillPolicy is validated by the config loader (exact Real/Tiered,
    // [DEFAULT] only); resolved here before the engine exists so the two
    // price-key semantics never mix. (tiered plan §3.6)
    let policy = match first.get("FillPolicy") {
        Some("Tiered") => FillPolicy::Tiered,
        Some("Real") | None => FillPolicy::Real,
        Some(other) => die(&format!("FillPolicy must be Real or Tiered, got {other:?}")),
    };
    let engine = Arc::new(Mutex::new(Engine::with_fill_policy(policy)));
    if let Err(e) = engine.lock().unwrap().load_instruments(&instruments_path) {
        die(&format!(
            "unable to load instruments {instruments_path}: {e}"
        ));
    }
    log::info!(
        "fill policy: {:?}{}",
        policy,
        if matches!(policy, FillPolicy::Tiered) {
            format!(" (MAX_TIER_FILLS={MAX_TIER_FILLS})")
        } else {
            String::new()
        }
    );
    let mut base_log = match LogConfig::from_settings(first, "logs/exchange") {
        Ok(log) => log,
        Err(error) => die_config(error),
    };
    base_log.dir = runtime::resolve(&root, &base_log.dir)
        .to_string_lossy()
        .into_owned();
    let dynamic = match first.bool("DynamicSessions", false) {
        Ok(value) => value,
        Err(error) => die_config(error),
    };
    let mut targets = Vec::new();
    let mut session_logs = std::collections::HashMap::new();
    for session in &sessions {
        if let Err(error) = FixConfig::validate_supported_runtime(session) {
            die_config(error);
        }
        if required(session, "BeginString", "acceptor [SESSION]") != begin_string
            || required(session, "SenderCompID", "acceptor [SESSION]") != sender_comp_id
        {
            die("all acceptor SESSION rows must use the same BeginString and SenderCompID");
        }
        let session_port = match session.required_u16("acceptor [SESSION]", "SocketAcceptPort") {
            Ok(value) => value,
            Err(error) => die_config(error),
        };
        if session_port != port {
            die("all acceptor SESSION rows must use the same SocketAcceptPort");
        }
        let mut session_log = match LogConfig::from_settings(session, "logs/exchange") {
            Ok(log) => log,
            Err(error) => die_config(error),
        };
        session_log.dir = runtime::resolve(&root, &session_log.dir)
            .to_string_lossy()
            .into_owned();
        if session_log.enabled {
            // Preflight both daily files before exposing any service.
            SessionLog::new(
                &session_log.dir,
                &begin_string,
                &sender_comp_id,
                session.get("TargetCompID").unwrap_or("preflight"),
            )
            .unwrap_or_else(|e| {
                die(&format!(
                    "FIX log directory {} is not writable: {e}",
                    session_log.dir
                ))
            });
        }
        if let Some(target) = session.get("TargetCompID") {
            session_logs.insert(target.to_string(), session_log.clone());
            targets.push(target.to_string());
        } else if !dynamic {
            die_config(ConfigError::Missing {
                section: "acceptor [SESSION]".to_string(),
                key: "TargetCompID".to_string(),
            });
        }
        if let Some(declared) = session.get("TargetCompIDs") {
            targets.extend(
                declared
                    .split(',')
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
            );
        }
    }
    targets.sort();
    targets.dedup();
    let admission = if dynamic {
        Admission::Dynamic
    } else {
        if targets.is_empty() {
            die("acceptor requires at least one TargetCompID when DynamicSessions=N");
        }
        Admission::Declared(targets)
    };
    let acceptor_cfg = AcceptorConfig {
        port,
        sender_comp_id,
        begin_string,
        log: base_log,
        session_logs,
        admission,
    };

    if let Some(path) = &ready_path {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            runtime::check_writable_dir(parent)
                .unwrap_or_else(|e| die(&format!("readiness directory is not writable: {e}")));
        }
        if path.exists() {
            die("readiness file already exists; remove stale instance state before starting");
        }
    }
    if check_only {
        println!("configuration and log permissions OK (FIX :{port}, REST :{rest_port}); no listeners opened");
        return;
    }
    let listener = bind_acceptor(&acceptor_cfg)
        .unwrap_or_else(|e| die(&format!("unable to bind FIX listener: {e}")));

    // read-only REST api
    if let Err(e) = rest::start(Arc::clone(&engine), &format!("0.0.0.0:{rest_port}")) {
        die(&format!("unable to start web server: {e}"));
    }
    let _ready = ready_path.as_ref().map(|path| {
        ReadyFile::create(path)
            .unwrap_or_else(|e| die(&format!("unable to publish readiness: {e}")))
    });
    log::info!("ready: FIX :{port}, REST :{rest_port}");

    // Service mode keeps the listener on the main thread and never reads stdin.
    // The service manager owns process lifetime; this does not daemonize/fork.
    if server_mode {
        if let Err(e) = run_acceptor_on(engine, acceptor_cfg, listener) {
            die(&format!("FIX acceptor failed: {e}"));
        }
        return;
    }

    // FIX acceptor
    {
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || {
            if let Err(e) = run_acceptor_on(engine, acceptor_cfg, listener) {
                log::error!("FIX acceptor failed: {}", e);
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
            Some("help") => {
                println!("The available commands are: quit, sessions, book SYMBOL, list")
            }
            Some("quit") => break,
            Some("sessions") => {
                println!(
                    "Active sessions: {}",
                    engine.lock().unwrap().session_ids().join(",")
                );
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

fn required(settings: &SessionSettings, key: &str, section: &str) -> String {
    settings
        .get(key)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            die_config(ConfigError::Missing {
                section: section.to_string(),
                key: key.to_string(),
            })
        })
}

fn die_config(error: ConfigError) -> ! {
    die(&error.to_string())
}

fn die(message: &str) -> ! {
    runtime::fatal(message)
}

fn format_book(symbol: &str, book: &Book) -> String {
    let levels = |levels: &[rust_trader::core::orderbook::BookLevel]| -> String {
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
