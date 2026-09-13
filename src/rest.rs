use std::sync::{Arc, Mutex};

use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;

use crate::core::exchange::Engine;

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

/// handle one request; pure so it can be unit tested without sockets
pub fn respond(engine: &Engine, path: &str) -> (u16, String) {
    if path.starts_with("/api/instruments/") || path == "/api/instruments" {
        let body = serde_json::to_string(&engine.all_symbols()).unwrap();
        return (200, body);
    }
    if let Some(symbol) = path.strip_prefix("/api/book/") {
        if symbol.is_empty() {
            return (404, "404 page not found".to_string());
        }
        if engine.instrument_by_symbol(symbol).is_none() {
            return (404, format!("the symbol {} is unknown\n", symbol));
        }
        let book = engine.book(symbol);
        let dto = match book {
            Some(book) => BookDto {
                symbol: symbol.to_string(),
                sequence: book.sequence,
                bids: book
                    .bids
                    .iter()
                    .map(|l| LevelDto { price: to_f64(l.price), quantity: to_f64(l.quantity) })
                    .collect(),
                asks: book
                    .asks
                    .iter()
                    .map(|l| LevelDto { price: to_f64(l.price), quantity: to_f64(l.quantity) })
                    .collect(),
            },
            None => BookDto {
                symbol: symbol.to_string(),
                sequence: 0,
                bids: vec![],
                asks: vec![],
            },
        };
        return (200, serde_json::to_string(&dto).unwrap());
    }
    if let Some(symbol) = path.strip_prefix("/api/stats/") {
        if symbol.is_empty() {
            return (404, "404 page not found".to_string());
        }
        if engine.instrument_by_symbol(symbol).is_none() {
            return (404, format!("the symbol {} is unknown\n", symbol));
        }
        let stats = engine.statistics(symbol);
        let dto = match stats {
            Some(stats) => StatsDto {
                symbol: stats.symbol,
                bid_price: to_f64(stats.bid_price),
                bid_qty: to_f64(stats.bid_qty),
                ask_price: to_f64(stats.ask_price),
                ask_qty: to_f64(stats.ask_qty),
                volume: to_f64(stats.volume),
                high: to_f64(stats.high),
                low: to_f64(stats.low),
                has_high_low: stats.has_high_low,
            },
            None => StatsDto {
                symbol: symbol.to_string(),
                bid_price: 0.0,
                bid_qty: 0.0,
                ask_price: 0.0,
                ask_qty: 0.0,
                volume: 0.0,
                high: 0.0,
                low: 0.0,
                has_high_low: false,
            },
        };
        return (200, serde_json::to_string(&dto).unwrap());
    }
    if path == "/api/sessions" {
        let body = serde_json::to_string(&engine.session_ids()).unwrap();
        return (200, body);
    }
    (404, "404 page not found".to_string())
}

/// start the REST server: one accept loop, N worker threads sharing the server
pub fn start(engine: Arc<Mutex<Engine>>, addr: &str) -> std::io::Result<std::thread::JoinHandle<()>> {
    let server = Arc::new(tiny_http::Server::http(addr).map_err(std::io::Error::other)?);
    let handle = std::thread::Builder::new()
        .name("rest".to_string())
        .spawn(move || {
            let mut workers = Vec::new();
            for i in 0..4 {
                let server = Arc::clone(&server);
                let engine = Arc::clone(&engine);
                let worker = std::thread::Builder::new()
                    .name(format!("rest-worker-{}", i))
                    .spawn(move || loop {
                        let mut request = match server.recv() {
                            Ok(request) => request,
                            Err(_) => continue,
                        };
                        let path = request.url().to_string();
                        let (status, body) = {
                            let engine = engine.lock().unwrap();
                            respond(&engine, &path)
                        };
                        let content_type = if body.starts_with('{') || body.starts_with('[') {
                            "application/json"
                        } else {
                            "text/plain; charset=utf-8"
                        };
                        let response = tiny_http::Response::from_string(body)
                            .with_status_code(status)
                            .with_header(
                                tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type).unwrap(),
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
    use std::sync::mpsc;

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
