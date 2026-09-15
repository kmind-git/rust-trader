use std::sync::{Arc, Mutex};

use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;

use crate::core::exchange::Engine;
use crate::market_data::{InstrumentSnapshot, MarketDataReader};

/// read-only REST api mirroring the Go implementation's endpoints and JSON
/// field names exactly (internal/exchange/webserver.go).

#[derive(Serialize)]
struct LevelDto {
    price: f64,
    quantity: f64,
}

#[derive(Serialize)]
struct BookDto {
    symbol: String,
    sequence: u64,
    bids: Vec<LevelDto>,
    asks: Vec<LevelDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatsDto {
    symbol: String,
    bid_price: f64,
    bid_qty: f64,
    ask_price: f64,
    ask_qty: f64,
    volume: f64,
    high: f64,
    low: f64,
    has_high_low: bool,
}

fn to_f64(d: rust_decimal::Decimal) -> f64 {
    d.to_f64().unwrap_or(0.0)
}

enum Snapshot {
    Strings(Vec<String>),
    Book(String, Arc<InstrumentSnapshot>),
    Stats(String, Arc<InstrumentSnapshot>),
    Missing(String),
}

/// Resolve a market-data request through the immutable read handle.
///
/// `MarketDataReader::snapshot` returns `None` only for an unknown symbol. A
/// known instrument with no orders still has an empty, sequence-zero
/// snapshot, so this preserves the old REST distinction between an empty book
/// and a 404 response without touching the engine mutex.
fn market_snapshot(reader: &MarketDataReader, path: &str) -> Snapshot {
    if path.starts_with("/api/instruments/") || path == "/api/instruments" {
        return Snapshot::Strings(reader.all_symbols());
    }
    for (prefix, book) in [("/api/book/", true), ("/api/stats/", false)] {
        if let Some(symbol) = path.strip_prefix(prefix) {
            if symbol.is_empty() {
                break;
            }
            let Some(market) = reader.snapshot(symbol) else {
                return Snapshot::Missing(format!("the symbol {} is unknown\n", symbol));
            };
            return if book {
                Snapshot::Book(symbol.into(), market)
            } else {
                Snapshot::Stats(symbol.into(), market)
            };
        }
    }
    Snapshot::Missing("404 page not found".into())
}

/// Route one request as the REST workers do.
///
/// The engine mutex is supplied because this is the actual worker routing
/// function. It is acquired only for `/api/sessions`; market endpoints use the
/// read handle and therefore do not dereference or lock the engine.
fn request_snapshot(reader: &MarketDataReader, engine: &Mutex<Engine>, path: &str) -> Snapshot {
    if path == "/api/sessions" {
        let engine = engine.lock().unwrap();
        return Snapshot::Strings(engine.session_ids());
    }
    market_snapshot(reader, path)
}

fn serialize(snapshot: Snapshot) -> (u16, String) {
    let body = match snapshot {
        Snapshot::Missing(message) => return (404, message),
        Snapshot::Strings(values) => serde_json::to_string(&values).unwrap(),
        Snapshot::Book(symbol, market) => {
            let levels = |items: &[crate::core::orderbook::BookLevel]| {
                items
                    .iter()
                    .map(|l| LevelDto {
                        price: to_f64(l.price),
                        quantity: to_f64(l.quantity),
                    })
                    .collect()
            };
            let dto = BookDto {
                symbol,
                sequence: market.book.sequence,
                bids: levels(&market.book.bids),
                asks: levels(&market.book.asks),
            };
            serde_json::to_string(&dto).unwrap()
        }
        Snapshot::Stats(symbol, market) => {
            let dto = StatsDto {
                symbol,
                bid_price: to_f64(market.stats.bid_price),
                bid_qty: to_f64(market.stats.bid_qty),
                ask_price: to_f64(market.stats.ask_price),
                ask_qty: to_f64(market.stats.ask_qty),
                volume: to_f64(market.stats.volume),
                high: to_f64(market.stats.high),
                low: to_f64(market.stats.low),
                has_high_low: market.stats.has_high_low,
            };
            serde_json::to_string(&dto).unwrap()
        }
    };
    (200, body)
}

/// Compatibility helper for callers without a shared mutex.
pub fn respond(engine: &Engine, path: &str) -> (u16, String) {
    let reader = engine.market_data();
    let data = if path == "/api/sessions" {
        Snapshot::Strings(engine.session_ids())
    } else {
        market_snapshot(&reader, path)
    };
    serialize(data)
}

/// start the REST server: one accept loop, N worker threads sharing the server
pub fn start(
    engine: Arc<Mutex<Engine>>,
    addr: &str,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let server = Arc::new(tiny_http::Server::http(addr).map_err(std::io::Error::other)?);
    // The handle is independent of the engine mutex. It observes newly
    // created instruments through the shared registry while each worker can
    // serve market data without locking the matching engine.
    let market_data = {
        let engine = engine.lock().unwrap();
        engine.market_data()
    };
    let handle = std::thread::Builder::new()
        .name("rest".to_string())
        .spawn(move || {
            let mut workers = Vec::new();
            for i in 0..4 {
                let server = Arc::clone(&server);
                let engine = Arc::clone(&engine);
                let market_data = market_data.clone();
                let worker = std::thread::Builder::new()
                    .name(format!("rest-worker-{}", i))
                    .spawn(move || loop {
                        let request = match server.recv() {
                            Ok(request) => request,
                            Err(_) => continue,
                        };
                        let path = request.url().to_string();
                        let data = request_snapshot(&market_data, &engine, &path);
                        let (status, body) = serialize(data);
                        let content_type = if body.starts_with('{') || body.starts_with('[') {
                            "application/json"
                        } else {
                            "text/plain; charset=utf-8"
                        };
                        let response = tiny_http::Response::from_string(body)
                            .with_status_code(status)
                            .with_header(
                                tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type)
                                    .unwrap(),
                            );
                        let _ = request.respond(response);
                    })
                    .expect("failed to spawn rest worker");
                workers.push(worker);
            }
            for w in workers {
                let _ = w.join();
            }
        })
        .expect("failed to spawn rest thread");
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::exchange::NewOrder;
    use crate::core::order::{OrderType, Side};
    use crate::queue as mpsc;

    fn engine_with_market() -> Engine {
        let mut engine = Engine::new();
        let (tx, _rx) = mpsc::channel();
        engine.register_session("mm", tx);
        let id = engine.create_instrument("IBM");
        engine
            .quote(
                "mm",
                id,
                "99.75".parse().unwrap(),
                "20".parse().unwrap(),
                "100".parse().unwrap(),
                "10".parse().unwrap(),
            )
            .unwrap();
        engine
            .create_order(
                "mm",
                NewOrder {
                    id: 1,
                    instrument_id: id,
                    side: Side::Buy,
                    order_type: OrderType::Limit,
                    price: "100.50".parse().unwrap(),
                    quantity: "5".parse().unwrap(),
                },
            )
            .unwrap();
        engine
    }

    #[test]
    fn snapshot_survives_engine_change_and_serializes_without_lock() {
        let mut engine = engine_with_market();
        let reader = engine.market_data();
        let old = reader.snapshot("IBM").expect("IBM snapshot");
        assert!(!old.book.asks.is_empty());

        engine.session_disconnect("mm");

        let current = reader.snapshot("IBM").expect("IBM snapshot");
        assert!(current.book.asks.is_empty());
        assert!(current.version > old.version);
        let shared = Mutex::new(engine);
        let (status, body) = serialize(request_snapshot(&reader, &shared, "/api/book/IBM"));
        assert_eq!(status, 200);
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(json["asks"].as_array().unwrap().is_empty());
        let (_, old_body) = serialize(Snapshot::Book("IBM".into(), old));
        let old_json: serde_json::Value = serde_json::from_str(&old_body).unwrap();
        assert!(!old_json["asks"].as_array().unwrap().is_empty());
    }

    #[test]
    fn market_route_completes_while_engine_mutex_is_held() {
        let shared = Arc::new(Mutex::new(engine_with_market()));
        let reader = {
            let engine = shared.lock().unwrap();
            engine.market_data()
        };
        let engine_guard = shared.lock().unwrap();

        // This is the same route function used by workers. The route itself
        // decides whether the Engine mutex is needed; market paths do not
        // acquire it.
        let (tx, rx) = std::sync::mpsc::channel();
        let querying = shared.clone();
        let worker = std::thread::spawn(move || {
            let responses: Vec<_> = [
                "/api/book/IBM",
                "/api/stats/IBM",
                "/api/instruments",
                "/bad",
            ]
            .into_iter()
            .map(|path| serialize(request_snapshot(&reader, &querying, path)))
            .collect();
            let _ = tx.send(responses);
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(2));
        // Release before joining so a locking regression fails instead of
        // leaving the test process deadlocked forever.
        drop(engine_guard);
        worker.join().unwrap();
        let responses = result.expect("market routes must not wait for the Engine mutex");
        assert_eq!(
            responses.iter().map(|r| r.0).collect::<Vec<_>>(),
            [200, 200, 200, 404]
        );
        assert!(responses[0].1.contains("\"symbol\":\"IBM\""));
    }

    #[test]
    fn old_reader_discovers_new_instruments() {
        let mut engine = Engine::new();
        let reader = engine.market_data();
        assert!(reader.all_symbols().is_empty());

        engine.create_instrument("IBM");

        assert_eq!(reader.all_symbols(), vec!["IBM".to_string()]);
    }

    #[test]
    fn known_empty_and_unknown_symbols_keep_rest_semantics() {
        let mut engine = Engine::new();
        engine.create_instrument("EMPTY");
        let reader = engine.market_data();

        let empty = reader.snapshot("EMPTY").expect("known empty instrument");
        assert_eq!(empty.version, 0);
        assert_eq!(empty.book.sequence, 0);
        assert!(empty.book.bids.is_empty());
        assert!(empty.book.asks.is_empty());

        let shared = Mutex::new(engine);
        let (status, body) = serialize(request_snapshot(&reader, &shared, "/api/book/EMPTY"));
        assert_eq!(status, 200);
        assert_eq!(
            body,
            r#"{"symbol":"EMPTY","sequence":0,"bids":[],"asks":[]}"#
        );

        let (status, body) = serialize(request_snapshot(&reader, &shared, "/api/book/UNKNOWN"));
        assert_eq!(status, 404);
        assert_eq!(body, "the symbol UNKNOWN is unknown\n");
    }

    #[test]
    fn test_instruments() {
        let engine = engine_with_market();
        let (status, body) = respond(&engine, "/api/instruments/");
        assert_eq!(status, 200);
        assert_eq!(body, r#"["IBM"]"#);
    }

    #[test]
    fn test_book() {
        let engine = engine_with_market();
        let (status, body) = respond(&engine, "/api/book/IBM");
        assert_eq!(status, 200);
        assert_eq!(
            body,
            r#"{"symbol":"IBM","sequence":2,"bids":[{"price":99.75,"quantity":20.0}],"asks":[{"price":100.0,"quantity":5.0}]}"#
        );
        let (status, body) = respond(&engine, "/api/book/UNKNOWN");
        assert_eq!(status, 404);
        assert_eq!(body, "the symbol UNKNOWN is unknown\n");
    }

    #[test]
    fn test_stats() {
        let engine = engine_with_market();
        let (status, body) = respond(&engine, "/api/stats/IBM");
        assert_eq!(status, 200);
        assert_eq!(
            body,
            r#"{"symbol":"IBM","bidPrice":99.75,"bidQty":20.0,"askPrice":100.0,"askQty":5.0,"volume":5.0,"high":100.0,"low":100.0,"hasHighLow":true}"#
        );
    }

    #[test]
    fn test_sessions_and_404() {
        let engine = engine_with_market();
        let (status, body) = respond(&engine, "/api/sessions");
        assert_eq!(status, 200);
        assert_eq!(body, r#"["mm"]"#);
        let (status, _) = respond(&engine, "/api/nothing");
        assert_eq!(status, 404);
    }
}
