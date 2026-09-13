use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc;

use rust_decimal::Decimal;

use super::instrument::{Instrument, InstrumentMap};
use super::order::{Order, OrderId, OrderState, OrderType, Side};
use super::orderbook::{Book, OrderBook, RawTrade};
use super::stats::Statistics;

pub const QUOTE_ORDER_ID: OrderId = 0;

#[derive(Debug, PartialEq, Eq)]
pub enum EngineError {
    OrderNotFound,
    OrderIsNotActive,
    UnknownSymbol(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::OrderNotFound => write!(f, "order not found"),
            EngineError::OrderIsNotActive => write!(f, "order is not active"),
            EngineError::UnknownSymbol(s) => write!(f, "unknown symbol {}", s),
        }
    }
}

/// a report pushed to a session's connection thread (order status or fill).
/// These are snapshots: no shared mutable state crosses threads.
#[derive(Clone, Debug)]
pub enum Report {
    Status { order: Order, symbol: String },
    Fill { order: Order, symbol: String, last_price: Decimal, last_quantity: Decimal },
}

/// an incoming order request from a client
#[derive(Clone, Debug)]
pub struct NewOrder {
    pub id: OrderId,
    pub instrument_id: i64,
    pub side: Side,
    pub order_type: OrderType,
    pub price: Decimal,
    pub quantity: Decimal,
}

/// the client-side record of a quote pair (bid & ask); re-quoting removes the
/// old pair from the books. Mirrors quotePair.
#[derive(Default)]
struct QuotePair {
    bid: Option<Order>,
    ask: Option<Order>,
}

struct Session {
    orders: HashMap<OrderId, Order>,
    quotes: HashMap<i64, QuotePair>,
    sender: mpsc::Sender<Report>,
}

/// the exchange engine. A single global lock guards all state (books, sessions,
/// caches); client threads lock per command and communicate results back over
/// per-session channels. This mirrors the Go implementation's observable
/// behavior while keeping ownership single-threaded inside the lock.
pub struct Engine {
    instruments: InstrumentMap,
    books: HashMap<i64, OrderBook>,
    sessions: BTreeMap<String, Session>,
    next_exchange_id: i32,
    next_trade_id: i64,
    next_arrival: u64,
    sequence: u64,
    book_cache: HashMap<i64, Book>,
    stats_cache: HashMap<i64, Statistics>,
}

impl Engine {
    pub fn new() -> Engine {
        Engine {
            instruments: InstrumentMap::new(),
            books: HashMap::new(),
            sessions: BTreeMap::new(),
            next_exchange_id: 0,
            next_trade_id: 0,
            next_arrival: 0,
            sequence: 0,
            book_cache: HashMap::new(),
            stats_cache: HashMap::new(),
        }
    }

    pub fn load_instruments(&mut self, path: &str) -> std::io::Result<()> {
        self.instruments.load(path)
    }

    // --- sessions ---

    pub fn register_session(&mut self, id: &str, sender: mpsc::Sender<Report>) {
        let session = Session { orders: HashMap::new(), quotes: HashMap::new(), sender };
        self.sessions.insert(id.to_string(), session);
    }

    pub fn session_ids(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }

    /// cancels all orders and quotes of a session (mirrors SessionDisconnect)
    pub fn session_disconnect(&mut self, session_id: &str) {
        let Some(session) = self.sessions.remove(session_id) else { return };
        let mut order_count = 0;
        let mut quote_count = 0;
        for (id, entry) in session.orders {
            if let Some(book) = self.books.get_mut(&entry.instrument_id) {
                if book.remove(session_id, id, entry.side, entry.price).is_ok() {
                    order_count += 1;
                }
            }
            let book = self.build_book(entry.instrument_id);
            self.record_market_data(book, &[]);
        }
        for (instrument_id, pair) in session.quotes {
            for leg in [pair.bid, pair.ask] {
                if let Some(order) = leg {
                    if let Some(book) = self.books.get_mut(&instrument_id) {
                        if book.remove(session_id, order.id, order.side, order.price).is_ok() {
                            quote_count += 1;
                        }
                    }
                }
            }
            let book = self.build_book(instrument_id);
            self.record_market_data(book, &[]);
        }
        log::info!(
            "session {} disconnected, cancelled {} orders {} quotes",
            session_id, order_count, quote_count
        );
    }

    // --- instruments ---

    pub fn create_instrument(&mut self, symbol: &str) -> i64 {
        if let Some(instrument) = self.instruments.get_by_symbol(symbol) {
            return instrument.id;
        }
        let id = self.instruments.next_id();
        self.instruments.put(Instrument::new(id, symbol));
        id
    }

    pub fn instrument_by_symbol(&self, symbol: &str) -> Option<&Instrument> {
        self.instruments.get_by_symbol(symbol)
    }

    pub fn all_symbols(&self) -> Vec<String> {
        self.instruments.all_symbols()
    }

    // --- orders ---

    /// mirrors exchange.CreateOrder
    pub fn create_order(&mut self, session_id: &str, new: NewOrder) -> Result<OrderId, EngineError> {
        if self.instruments.get_by_id(new.instrument_id).is_none() {
            return Err(EngineError::UnknownSymbol(new.instrument_id.to_string()));
        }
        self.next_exchange_id += 1;
        self.next_arrival += 1;
        let mut order = match new.order_type {
            OrderType::Limit => Order::limit(
                session_id, new.id, new.instrument_id, new.side, new.price, new.quantity, self.next_arrival,
            ),
            OrderType::Market => Order::market(
                session_id, new.id, new.instrument_id, new.side, new.quantity, self.next_arrival,
            ),
        };
        order.exchange_id = self.next_exchange_id.to_string();

        let session = self.sessions.get_mut(session_id).ok_or(EngineError::OrderNotFound)?;
        session.orders.insert(order.id, order.clone());

        let (mut trades, final_state) = self.add_to_book(order);
        let book = self.build_book(new.instrument_id);
        self.record_market_data(book, &trades);
        self.send_trade_reports(&mut trades);
        if let Some(final_order) = &final_state {
            self.sync_record(&final_order.clone());
            // mirror Go: status goes out when nothing traded, OR when a market
            // order's unfilled remainder was cancelled
            let remainder_cancelled =
                final_order.state == OrderState::Cancelled && new.order_type == OrderType::Market;
            if trades.is_empty() || remainder_cancelled {
                self.send_status(final_order.clone());
            }
        }
        Ok(new.id)
    }

    /// mirrors exchange.ModifyOrder: remove + re-add, so time priority resets.
    /// The replacement order takes the cancel-replace request's ClOrdID
    /// (new_id); when it differs from the original, the original order is
    /// reported Cancelled under its own id first. Reusing the same id keeps
    /// the old single-report in-place semantics (Go-style clients).
    pub fn modify_order(
        &mut self,
        session_id: &str,
        orig_id: OrderId,
        new_id: OrderId,
        price: Decimal,
        quantity: Decimal,
    ) -> Result<(), EngineError> {
        let entry = {
            let session = self.sessions.get(session_id).ok_or(EngineError::OrderNotFound)?;
            session.orders.get(&orig_id).ok_or(EngineError::OrderNotFound)?.clone()
        };
        if !entry.state.is_active() {
            return Err(EngineError::OrderIsNotActive);
        }
        let instrument_id = entry.instrument_id;

        let removed = match self.remove_from_book(session_id, orig_id, entry.side, entry.price, instrument_id) {
            Ok(removed) => removed,
            Err(_) => {
                // order exists in the session but not in the book: report and
                // ignore, mirroring the Go behavior
                let record = self.sessions.get(session_id).unwrap().orders.get(&orig_id).unwrap().clone();
                self.send_status(record);
                return Ok(());
            }
        };
        self.record_book(instrument_id);

        if new_id != orig_id {
            // the original order is superseded: terminal report under its id
            let mut old = removed;
            if old.state.is_active() {
                old.state = OrderState::Cancelled;
            }
            self.sessions.get_mut(session_id).unwrap().orders.remove(&orig_id);
            self.send_status(old);
        }

        self.next_arrival += 1;
        let mut order = entry;
        order.id = new_id;
        order.price = price;
        order.quantity = quantity;
        order.remaining = quantity;
        order.state = OrderState::Booked;
        order.arrival = self.next_arrival;

        self.sessions
            .get_mut(session_id)
            .unwrap()
            .orders
            .insert(new_id, order.clone());

        let (mut trades, final_state) = self.add_to_book(order);
        let book = self.build_book(instrument_id);
        self.record_market_data(book, &trades);
        self.send_trade_reports(&mut trades);
        if let Some(final_order) = &final_state {
            self.sync_record(&final_order.clone());
            if trades.is_empty() {
                self.send_status(final_order.clone());
            }
        }
        Ok(())
    }

    /// mirrors exchange.CancelOrder
    pub fn cancel_order(&mut self, session_id: &str, id: OrderId) -> Result<(), EngineError> {
        let entry = {
            let session = self.sessions.get(session_id).ok_or(EngineError::OrderNotFound)?;
            session.orders.get(&id).ok_or(EngineError::OrderNotFound)?.clone()
        };
        let mut removed = self.remove_from_book(session_id, id, entry.side, entry.price, entry.instrument_id)?;
        let instrument_id = removed.instrument_id;
        if removed.state.is_active() {
            removed.state = OrderState::Cancelled;
        }
        self.sessions
            .get_mut(session_id)
            .unwrap()
            .orders
            .insert(id, removed.clone());
        {
            let book = self.build_book(instrument_id);
            self.record_market_data(book, &[]);
        }
        self.send_status(removed);
        Ok(())
    }

    /// mirrors exchange.Quote: a bid/ask pair per session and instrument, with
    /// replace semantics. A zero price skips (or withdraws) that side.
    #[allow(clippy::too_many_arguments)]
    pub fn quote(
        &mut self,
        session_id: &str,
        instrument_id: i64,
        bid_price: Decimal,
        bid_quantity: Decimal,
        ask_price: Decimal,
        ask_quantity: Decimal,
    ) -> Result<(), EngineError> {
        if self.instruments.get_by_id(instrument_id).is_none() {
            return Err(EngineError::UnknownSymbol(instrument_id.to_string()));
        }
        let old_pair = {
            let session = self.sessions.get_mut(session_id).ok_or(EngineError::OrderNotFound)?;
            session.quotes.remove(&instrument_id).unwrap_or_default()
        };
        for leg in [old_pair.bid, old_pair.ask] {
            if let Some(order) = leg {
                let _ = self.remove_from_book(session_id, order.id, order.side, order.price, instrument_id);
            }
        }

        let mut all_trades: Vec<RawTrade> = Vec::new();
        let mut pair = QuotePair::default();

        for (price, quantity, side, exchange_id) in [
            (bid_price, bid_quantity, Side::Buy, format!("quote.bid.{}", instrument_id)),
            (ask_price, ask_quantity, Side::Sell, format!("quote.ask.{}", instrument_id)),
        ] {
            if price.is_zero() {
                continue;
            }
            self.next_exchange_id += 1;
            self.next_arrival += 1;
            let mut order = Order::limit(
                session_id, QUOTE_ORDER_ID, instrument_id, side, price, quantity, self.next_arrival,
            );
            order.exchange_id = exchange_id;
            let (trades, _) = self.add_to_book(order.clone());
            all_trades.extend(trades);
            match side {
                Side::Buy => pair.bid = Some(order),
                Side::Sell => pair.ask = Some(order),
            }
        }
        self.sessions.get_mut(session_id).unwrap().quotes.insert(instrument_id, pair);

        let book = self.build_book(instrument_id);
        self.record_market_data(book, &all_trades);
        self.send_trade_reports(&mut all_trades);
        Ok(())
    }

    // --- book & statistics access (REST / console) ---

    pub fn book(&self, symbol: &str) -> Option<Book> {
        let instrument = self.instruments.get_by_symbol(symbol)?;
        self.book_cache.get(&instrument.id).cloned()
    }

    pub fn statistics(&self, symbol: &str) -> Option<Statistics> {
        let instrument = self.instruments.get_by_symbol(symbol)?;
        self.stats_cache.get(&instrument.id).cloned()
    }

    // --- internals ---

    fn add_to_book(&mut self, order: Order) -> (Vec<RawTrade>, Option<Order>) {
        let instrument_id = order.instrument_id;
        let book = self.books.entry(instrument_id).or_insert_with(|| OrderBook::new(instrument_id));
        book.add(order)
    }

    fn remove_from_book(
        &mut self,
        session_id: &str,
        id: OrderId,
        side: Side,
        price: Decimal,
        instrument_id: i64,
    ) -> Result<Order, EngineError> {
        let book = self.books.get_mut(&instrument_id).ok_or(EngineError::OrderNotFound)?;
        book.remove(session_id, id, side, price).map_err(|_| EngineError::OrderNotFound)
    }

    fn build_book(&mut self, instrument_id: i64) -> Book {
        self.books
            .entry(instrument_id)
            .or_insert_with(|| OrderBook::new(instrument_id))
            .build_book()
    }

    /// cache the latest book (assigning a new sequence number) and fold trades
    /// into the running statistics. Mirrors recordMarketData in stats.go.
    fn record_market_data(&mut self, mut book: Book, trades: &[RawTrade]) {
        self.sequence += 1;
        book.sequence = self.sequence;
        let instrument_id = book.instrument_id;

        let stats = self.stats_cache.entry(instrument_id).or_insert_with(|| Statistics {
            symbol: self
                .instruments
                .get_by_id(instrument_id)
                .map(|i| i.symbol.clone())
                .unwrap_or_default(),
            ..Statistics::default()
        });

        if book.has_bids() {
            stats.bid_price = book.bids[0].price;
            stats.bid_qty = book.bids[0].quantity;
        }
        if book.has_asks() {
            stats.ask_price = book.asks[0].price;
            stats.ask_qty = book.asks[0].quantity;
        }
        for trade in trades {
            stats.volume += trade.quantity;
            if !stats.has_high_low {
                stats.high = trade.price;
                stats.low = trade.price;
                stats.has_high_low = true;
            } else {
                if trade.price > stats.high {
                    stats.high = trade.price;
                }
                if trade.price < stats.low {
                    stats.low = trade.price;
                }
            }
        }
        self.book_cache.insert(instrument_id, book);
    }

    /// convenience wrapper used where only the book changed
    fn record_book(&mut self, instrument_id: i64) {
        let book = self.build_book(instrument_id);
        self.record_market_data(book, &[]);
    }

    /// assign the batch trade id and push fill reports to both sides of every
    /// trade (mirrors SendTrades + the shared tradeid per match batch)
    fn send_trade_reports(&mut self, trades: &mut [RawTrade]) {
        if trades.is_empty() {
            return;
        }
        self.next_trade_id += 1;
        let trade_id = self.next_trade_id;
        for trade in trades.iter_mut() {
            trade.trade_id = trade_id;
            self.send_fill(&trade.buyer, trade.price, trade.quantity);
            self.send_fill(&trade.seller, trade.price, trade.quantity);
        }
    }

    fn send_fill(&mut self, order: &Order, last_price: Decimal, last_quantity: Decimal) {
        // update the session's mirrored record for regular orders; quote legs
        // (id 0) are not tracked in session.orders
        if order.id != QUOTE_ORDER_ID {
            if let Some(record) = self
                .sessions
                .get_mut(&order.session_id)
                .and_then(|s| s.orders.get_mut(&order.id))
            {
                record.remaining = order.remaining;
                record.state = order.state;
            }
        }
        let symbol = self
            .instruments
            .get_by_id(order.instrument_id)
            .map(|i| i.symbol.clone())
            .unwrap_or_default();
        self.send(Report::Fill { order: order.clone(), symbol, last_price, last_quantity });
    }

    /// keep the session's mirrored order record at the engine's final state
    /// (in Go this is a shared pointer, so it stays in sync automatically)
    fn sync_record(&mut self, order: &Order) {
        if order.id == QUOTE_ORDER_ID {
            return;
        }
        if let Some(record) = self
            .sessions
            .get_mut(&order.session_id)
            .and_then(|s| s.orders.get_mut(&order.id))
        {
            *record = order.clone();
        }
    }

    fn send_status(&mut self, order: Order) {
        let symbol = self
            .instruments
            .get_by_id(order.instrument_id)
            .map(|i| i.symbol.clone())
            .unwrap_or_default();
        self.send(Report::Status { order, symbol });
    }

    fn send(&mut self, report: Report) {
        let session_id = match &report {
            Report::Status { order, .. } => &order.session_id,
            Report::Fill { order, .. } => &order.session_id,
        };
        if let Some(session) = self.sessions.get(session_id) {
            // a send error means the connection thread is gone; ignore
            let _ = session.sender.send(report);
        }
    }
}

#[cfg(test)]
mod tests;
