//! FIX session layer. The acceptor (exchange side) and initiator (client
//! side) share the same framing and writer-thread design: the writer owns the
//! outbound sequence number, the reader validates inbound sequence numbers and
//! dispatches decoded messages.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use rust_decimal::Decimal;

use crate::core::exchange::{Engine, NewOrder, Report};
use crate::core::instrument::Instrument;
use crate::core::order::{OrderId, OrderState, OrderType, Side};

use super::codec::{self, msg_type, Inbound};
use super::frame;

// ---------- shared plumbing ----------

/// a boolean flag threads set and callers wait on with a timeout
/// (mirrors common.StatusBool)
#[derive(Clone)]
pub struct Signal(Arc<(Mutex<bool>, Condvar)>);

impl Signal {
    pub fn new() -> Signal {
        Signal(Arc::new((Mutex::new(false), Condvar::new())))
    }
    pub fn set(&self) {
        let (lock, cvar) = &*self.0;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }
    pub fn reset(&self) {
        let (lock, _) = &*self.0;
        *lock.lock().unwrap() = false;
    }
    pub fn get(&self) -> bool {
        *self.0 .0.lock().unwrap()
    }
    pub fn wait_for(&self, timeout: Duration) -> bool {
        let (lock, cvar) = &*self.0;
        let guard = cvar
            .wait_timeout_while(lock.lock().unwrap(), timeout, |v| !*v)
            .unwrap();
        *guard.0
    }
}

/// messages handed to the writer thread, which owns the outbound sequence
/// number and does the framing
pub enum OutMsg {
    Report(Report),
    Message { msg_type: &'static str, fields: Vec<(u32, String)> },
    /// sequence reset answering a ResendRequest: we persist nothing, so we
    /// tell the counterparty to jump to our next outbound sequence
    GapFill,
    Shutdown,
}

struct WriterState {
    stream: TcpStream,
    begin_string: String,
    sender_comp_id: String,
    target_comp_id: String,
    seq: u64,
}

fn writer_loop(mut state: WriterState, rx: Receiver<OutMsg>, idle: Duration) {
    loop {
        let out = match rx.recv_timeout(idle) {
            Ok(out) => out,
            Err(RecvTimeoutError::Timeout) => OutMsg::Message {
                msg_type: msg_type::HEARTBEAT,
                fields: vec![],
            },
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let (msg_type, fields) = match out {
            OutMsg::Shutdown => break,
            OutMsg::GapFill => {
                state.seq += 1;
                let this_seq = state.seq;
                (
                    msg_type::SEQUENCE_RESET,
                    vec![
                        (codec::tags::NEW_SEQ_NO, (this_seq + 1).to_string()),
                        (codec::tags::GAP_FILL_FLAG, "Y".to_string()),
                    ],
                )
            }
            OutMsg::Message { msg_type, fields } => (msg_type, fields),
            OutMsg::Report(report) => (msg_type::EXECUTION_REPORT, codec::build_execution_report(&report)),
        };
        state.seq += 1;
        let bytes = frame::frame(
            &state.begin_string,
            msg_type,
            state.seq,
            &state.sender_comp_id,
            &state.target_comp_id,
            &fields,
        );
        if state.stream.write_all(&bytes).is_err() || state.stream.flush().is_err() {
            break;
        }
    }
}

// ---------- acceptor (exchange side) ----------

#[derive(Clone)]
pub struct AcceptorConfig {
    pub port: u16,
    pub sender_comp_id: String,
    pub begin_string: String,
}

/// blocking accept loop; spawn this on its own thread
pub fn run_acceptor(engine: Arc<Mutex<Engine>>, cfg: AcceptorConfig) -> std::io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", cfg.port))?;
    log::info!("FIX acceptor listening on port {}", cfg.port);
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let engine = Arc::clone(&engine);
                let cfg = cfg.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle_acceptor_connection(stream, engine, cfg) {
                        log::warn!("connection ended: {}", e);
                    }
                });
            }
            Err(e) => log::warn!("accept failed: {}", e),
        }
    }
    Ok(())
}

fn handle_acceptor_connection(
    stream: TcpStream,
    engine: Arc<Mutex<Engine>>,
    cfg: AcceptorConfig,
) -> std::io::Result<()> {
    stream.set_nodelay(true).ok();
    let idle = Duration::from_secs(30);
    let mut reader = BufReader::new(stream.try_clone()?);
    reader.get_ref().set_read_timeout(Some(idle))?;
    let mut writer_stream = stream.try_clone()?;

    // logon handshake: first message must be a Logon addressed to us
    let logon = match frame::read_message(&mut reader)? {
        Some(message) => message,
        None => return Ok(()),
    };
    if logon.msg_type() != Some(msg_type::LOGON) {
        log::warn!("first message was not a Logon, closing");
        return Ok(());
    }
    if logon.begin_string != cfg.begin_string {
        log::warn!("unsupported FixVersion {}, closing", logon.begin_string);
        return Ok(());
    }
    let client_comp_id = match logon.get(49) {
        Some(v) => v.to_string(),
        None => return Ok(()),
    };
    if let Some(target) = logon.get(56) {
        if target != cfg.sender_comp_id {
            log::warn!("logon for unknown target {}, closing", target);
            return Ok(());
        }
    }
    let heart_bt_int: u32 = logon.get(codec::tags::HEART_BT_INT).and_then(|v| v.parse().ok()).unwrap_or(30);

    let session_id = format!("{}:{}->{}", cfg.begin_string, cfg.sender_comp_id, client_comp_id);
    let (tx, rx) = mpsc::channel::<OutMsg>();
    let (report_tx, report_rx) = mpsc::channel::<Report>();
    engine.lock().unwrap().register_session(&session_id, report_tx);
    {
        // pump engine reports into the writer channel
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("fix-report-pump".to_string())
            .spawn(move || {
                for report in report_rx {
                    if tx.send(OutMsg::Report(report)).is_err() {
                        break;
                    }
                }
            })
            .ok();
    }

    let begin_string = cfg.begin_string.clone();
    let sender_comp_id = cfg.sender_comp_id.clone();
    let writer = std::thread::Builder::new()
        .name(format!("fix-writer-{}", client_comp_id))
        .spawn(move || {
            let state = WriterState {
                stream: writer_stream,
                begin_string,
                sender_comp_id,
                target_comp_id: client_comp_id,
                seq: 0,
            };
            writer_loop(state, rx, idle.max(Duration::from_secs(5)))
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    // logon acknowledgement
    tx.send(OutMsg::Message {
        msg_type: msg_type::LOGON,
        fields: codec::build_logon(heart_bt_int),
    })
    .ok();
    log::info!("session {} logged on", session_id);

    let mut expected_in: u64 = 2; // the logon consumed sequence 1
    let mut timeouts = 0u32;
    loop {
        match frame::read_message(&mut reader) {
            Ok(None) => {
                log::warn!("session {}: peer closed connection", session_id);
                break;
            }
            Ok(Some(msg)) => {
                log::info!("session {}: inbound {} seq {:?} expected {}", session_id, msg.msg_type().unwrap_or("?"), msg.seq(), expected_in);
                timeouts = 0;
                if msg.begin_string != cfg.begin_string {
                    continue;
                }
                match msg.seq() {
                    Some(s) if s < expected_in => {
                        log::warn!("session {}: duplicate seq {}, expected {}", session_id, s, expected_in);
                        continue;
                    }
                    Some(s) if s > expected_in => {
                        log::warn!(
                            "session {}: seq gap (got {}, expected {}), requesting resend",
                            session_id, s, expected_in
                        );
                        tx.send(OutMsg::Message {
                            msg_type: msg_type::RESEND_REQUEST,
                            fields: vec![(7, expected_in.to_string()), (16, "0".to_string())],
                        })
                        .ok();
                        continue;
                    }
                    _ => {}
                }
                expected_in += 1;

                let decoded = match codec::decode(&msg) {
                    Ok(decoded) => decoded,
                    Err(e) => {
                        log::warn!("session {}: bad message: {}", session_id, e);
                        continue;
                    }
                };
                match decoded {
                    Inbound::Heartbeat => {}
                    Inbound::TestRequest { test_req_id } => {
                        tx.send(OutMsg::Message {
                            msg_type: msg_type::HEARTBEAT,
                            fields: codec::build_heartbeat(Some(&test_req_id)),
                        })
                        .ok();
                    }
                    Inbound::ResendRequest => {
                        tx.send(OutMsg::GapFill).ok();
                    }
                    Inbound::Logout => {
                        tx.send(OutMsg::Message { msg_type: msg_type::LOGOUT, fields: vec![] }).ok();
                        break;
                    }
                    Inbound::Logon { .. } => log::warn!("session {}: duplicate logon ignored", session_id),
                    Inbound::Unsupported(t) => log::warn!("session {}: unsupported msg type {}", session_id, t),
                    inbound => {
                        let mut engine = engine.lock().unwrap();
                        handle_business(&mut engine, &session_id, &tx, inbound);
                    }
                }
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut {
                    timeouts += 1;
                    if timeouts >= 2 {
                        log::warn!("session {}: no data for {}s, closing", session_id, idle.as_secs() * 2);
                        break;
                    }
                    tx.send(OutMsg::Message {
                        msg_type: msg_type::TEST_REQUEST,
                        fields: codec::build_test_request("ARE-YOU-THERE"),
                    })
                    .ok();
                    continue;
                }
                log::warn!("session {}: read error: {} (kind {:?})", session_id, e, e.kind());
                break;
            }
        }
    }
    engine.lock().unwrap().session_disconnect(&session_id);
    log::info!("session {} disconnected", session_id);
    drop(tx);
    let _ = writer.join();
    Ok(())
}

/// FIX 4.2 has no SessionReject; business-level problems go out as a
/// BusinessMessageReject (j) with the reason in Text (58)
fn reject_business(tx: &Sender<OutMsg>, reason: &str) {
    tx.send(OutMsg::Message {
        msg_type: msg_type::BUSINESS_MESSAGE_REJECT,
        fields: vec![(58, reason.to_string())],
    })
    .ok();
}

/// apply a business message to the engine
fn handle_business(engine: &mut Engine, session_id: &str, tx: &Sender<OutMsg>, inbound: Inbound) {
    match inbound {
        Inbound::NewOrderSingle { cl_ord_id, symbol, side, order_type, price, quantity } => {
            let instrument_id = match engine.instrument_by_symbol(&symbol) {
                Some(instrument) => instrument.id,
                None => {
                    log::warn!("unknown symbol {}", symbol);
                    reject_business(tx, &format!("unknown symbol {}", symbol));
                    return;
                }
            };
            if let Err(e) = engine.create_order(
                session_id,
                NewOrder { id: cl_ord_id, instrument_id, side, order_type, price, quantity },
            ) {
                log::warn!("create order failed: {}", e);
            }
        }
        Inbound::CancelRequest { cl_ord_id } => {
            if let Err(e) = engine.cancel_order(session_id, cl_ord_id) {
                log::warn!("cancel order failed: {}", e);
            }
        }
        Inbound::CancelReplace { cl_ord_id, price, quantity } => {
            if let Err(e) = engine.modify_order(session_id, cl_ord_id, price, quantity) {
                log::warn!("modify order failed: {}", e);
            }
        }
        Inbound::MassQuote { quote_id, ack, symbol, bid_px, bid_qty, offer_px, offer_qty } => {
            let instrument_id = match engine.instrument_by_symbol(&symbol) {
                Some(instrument) => instrument.id,
                None => {
                    log::warn!("unknown symbol {}", symbol);
                    reject_business(tx, &format!("unknown symbol {}", symbol));
                    return;
                }
            };
            if let Err(e) = engine.quote(session_id, instrument_id, bid_px, bid_qty, offer_px, offer_qty) {
                log::warn!("quote failed: {}", e);
                return;
            }
            if ack {
                tx.send(OutMsg::Message {
                    msg_type: msg_type::MASS_QUOTE_ACKNOWLEDGEMENT,
                    fields: codec::build_mass_quote_ack(&quote_id),
                })
                .ok();
            }
        }
        _ => {}
    }
}

// ---------- initiator (client side) ----------

/// client-side view of an order (mirrors what the Go connector tracks in
/// c.orders and hands to OnOrderStatus)
#[derive(Clone, Debug)]
pub struct OrderView {
    pub id: OrderId,
    pub exchange_id: String,
    pub symbol: String,
    pub side: Side,
    pub price: Decimal,
    pub quantity: Decimal,
    pub remaining: Decimal,
    pub state: OrderState,
}

#[derive(Clone, Debug)]
pub struct FillView {
    pub is_quote: bool,
    pub order_id: Option<OrderId>,
    pub exchange_id: String,
    pub symbol: String,
    pub side: Side,
    pub price: Decimal,
    pub quantity: Decimal,
}

pub trait Callback: Send {
    fn on_instrument(&mut self, _instrument: &Instrument) {}
    fn on_order_status(&mut self, _order: &OrderView) {}
    fn on_fill(&mut self, _fill: &FillView) {}
}

struct InitiatorShared {
    logged_in: Signal,
    connected: Signal,
    callback: Mutex<Box<dyn Callback + Send>>,
    orders: Mutex<HashMap<OrderId, OrderView>>,
    instruments: Mutex<crate::core::instrument::InstrumentMap>,
}

pub struct InitiatorConfig {
    pub sender_comp_id: String,
    pub target_comp_id: String,
    pub host: String,
    pub port: u16,
    pub heart_bt_int: u32,
}

pub struct Initiator {
    shared: Arc<InitiatorShared>,
    cmd_tx: Sender<OutMsg>,
    next_order: Mutex<i32>,
}

#[derive(Debug)]
pub enum ConnectError {
    Io(std::io::Error),
    Timeout,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Io(e) => write!(f, "{}", e),
            ConnectError::Timeout => write!(f, "connection failed"),
        }
    }
}

impl Initiator {
    /// connect, log on (blocking up to 30s), and start the session threads
    pub fn connect(cfg: InitiatorConfig, callback: Box<dyn Callback + Send>) -> Result<Initiator, ConnectError> {
        let stream = TcpStream::connect((cfg.host.as_str(), cfg.port)).map_err(ConnectError::Io)?;
        stream.set_nodelay(true).ok();
        let mut reader_stream = stream.try_clone().map_err(ConnectError::Io)?;
        reader_stream.set_read_timeout(Some(Duration::from_secs(90))).ok();
        let mut reader = BufReader::new(reader_stream);
        let writer_stream = stream.try_clone().map_err(ConnectError::Io)?;
        let (cmd_tx, cmd_rx) = mpsc::channel::<OutMsg>();

        let shared = Arc::new(InitiatorShared {
            logged_in: Signal::new(),
            connected: Signal::new(),
            callback: Mutex::new(callback),
            orders: Mutex::new(HashMap::new()),
            instruments: Mutex::new(crate::core::instrument::InstrumentMap::new()),
        });

        // writer thread
        {
            let state = WriterState {
                stream: writer_stream,
                begin_string: "FIX.4.2".to_string(),
                sender_comp_id: cfg.sender_comp_id.clone(),
                target_comp_id: cfg.target_comp_id.clone(),
                seq: 0,
            };
            let idle = Duration::from_secs(cfg.heart_bt_int.max(5) as u64);
            std::thread::Builder::new()
                .name("fix-writer".to_string())
                .spawn(move || writer_loop(state, cmd_rx, idle))
                .map_err(ConnectError::Io)?;
        }

        // reader thread
        {
            let shared = Arc::clone(&shared);
            // inbound messages are addressed to us: 56 must be our SenderCompID
            let expected_target = cfg.sender_comp_id.clone();
            let mut shutdown = cmd_tx.clone();
            std::thread::Builder::new()
                .name("fix-reader".to_string())
                .spawn(move || {
                    let mut expected_in: u64 = 1;
                    loop {
                        let message = match frame::read_message(&mut reader) {
                            Ok(Some(message)) => message,
                            Ok(None) => break,
                            Err(_) => break,
                        };
                        if message.get(56).map(|v| v != expected_target).unwrap_or(false) {
                            continue;
                        }
                        match message.seq() {
                            Some(s) if s < expected_in => {
                                log::warn!("duplicate seq {}", s);
                                continue;
                            }
                            Some(s) if s > expected_in => {
                                log::warn!("seq gap (got {}, expected {})", s, expected_in);
                                // process anyway: as a client we can't afford to stall
                            }
                            _ => {}
                        }
                        expected_in = message.seq().map(|s| s + 1).unwrap_or(expected_in);
                        let decoded = match codec::decode(&message) {
                            Ok(decoded) => decoded,
                            Err(e) => {
                                log::warn!("bad message: {}", e);
                                continue;
                            }
                        };
                        match decoded {
                            Inbound::Logon { .. } => shared.logged_in.set(),
                            Inbound::Logout => {
                                log::info!("we are logged out!");
                                shared.logged_in.reset();
                                break;
                            }
                            Inbound::Heartbeat => {}
                            Inbound::TestRequest { test_req_id } => {
                                shutdown
                                    .send(OutMsg::Message {
                                        msg_type: msg_type::HEARTBEAT,
                                        fields: codec::build_heartbeat(Some(&test_req_id)),
                                    })
                                    .ok();
                            }
                            Inbound::ResendRequest => { shutdown.send(OutMsg::GapFill).ok(); }
                            Inbound::SecurityDefinition { symbol, instrument_id, .. } => {
                                let instrument = Instrument::new(instrument_id, &symbol);
                                shared.instruments.lock().unwrap().put(instrument.clone());
                                shared.callback.lock().unwrap().on_instrument(&instrument);
                            }
                            Inbound::ExecutionReport(data) => {
                                handle_execution_report(&shared, data);
                            }
                            _ => {}
                        }
                    }
                    shared.logged_in.reset();
                    shared.connected.reset();
                    let _ = shutdown.send(OutMsg::Shutdown);
                })
                .map_err(ConnectError::Io)?;
        }

        let initiator = Initiator {
            shared,
            cmd_tx,
            next_order: Mutex::new(0),
        };
        // log on and wait
        initiator
            .cmd_tx
            .send(OutMsg::Message { msg_type: msg_type::LOGON, fields: codec::build_logon(cfg.heart_bt_int) })
            .map_err(|_| ConnectError::Timeout)?;
        if !initiator.shared.logged_in.wait_for(Duration::from_secs(30)) {
            return Err(ConnectError::Timeout);
        }
        initiator.shared.connected.set();
        Ok(initiator)
    }

    pub fn is_connected(&self) -> bool {
        self.shared.connected.get()
    }

    /// look up a downloaded instrument's numeric id
    pub fn instrument_id(&self, symbol: &str) -> Option<i64> {
        self.shared
            .instruments
            .lock()
            .unwrap()
            .get_by_symbol(symbol)
            .map(|i| i.id)
    }

    /// graceful disconnect: Logout then Shutdown flow through the same FIFO
    /// channel as order traffic, so pending messages are flushed first
    pub fn disconnect(&mut self) {
        if self.shared.connected.get() {
            self.cmd_tx
                .send(OutMsg::Message { msg_type: msg_type::LOGOUT, fields: codec::build_logout() })
                .ok();
        }
        self.cmd_tx.send(OutMsg::Shutdown).ok();
        self.shared.logged_in.reset();
        self.shared.connected.reset();
        std::thread::sleep(Duration::from_millis(150));
    }

    fn require_connected(&self) -> Result<(), ()> {
        if self.shared.logged_in.get() {
            Ok(())
        } else {
            Err(())
        }
    }

    pub fn create_order(
        &self,
        symbol: &str,
        side: Side,
        order_type: OrderType,
        price: Decimal,
        quantity: Decimal,
    ) -> Result<OrderId, ()> {
        self.require_connected().map_err(|_| ())?;
        let mut next_order = self.next_order.lock().unwrap();
        *next_order += 1;
        let id = *next_order;
        self.shared.orders.lock().unwrap().insert(
            id,
            OrderView {
                id,
                exchange_id: String::new(),
                symbol: symbol.to_string(),
                side,
                price,
                quantity,
                remaining: quantity,
                state: OrderState::New,
            },
        );
        drop(next_order);
        let fields = codec::build_new_order_single(
            codec::OrderIdForWire(id.to_string()),
            symbol,
            side,
            order_type,
            price,
            quantity,
        );
        self.cmd_tx
            .send(OutMsg::Message { msg_type: msg_type::NEW_ORDER_SINGLE, fields })
            .map_err(|_| ())?;
        Ok(id)
    }

    pub fn modify_order(&self, id: OrderId, price: Decimal, quantity: Decimal) -> Result<(), ()> {
        self.require_connected().map_err(|_| ())?;
        let record = {
            let mut orders = self.shared.orders.lock().unwrap();
            let record = orders.get_mut(&id).ok_or(())?;
            record.price = price;
            record.quantity = quantity;
            record.clone()
        };
        let fields =
            codec::build_cancel_replace(&id.to_string(), &record.symbol, record.side, price, quantity);
        self.cmd_tx
            .send(OutMsg::Message { msg_type: msg_type::ORDER_CANCEL_REPLACE_REQUEST, fields })
            .map_err(|_| ())
    }

    pub fn cancel_order(&self, id: OrderId) -> Result<(), ()> {
        self.require_connected().map_err(|_| ())?;
        let record = {
            let orders = self.shared.orders.lock().unwrap();
            let record = orders.get(&id).ok_or(())?.clone();
            record
        };
        let fields = codec::build_cancel_request(&id.to_string(), &record.symbol, record.side);
        self.cmd_tx
            .send(OutMsg::Message { msg_type: msg_type::ORDER_CANCEL_REQUEST, fields })
            .map_err(|_| ())
    }

    pub fn quote(
        &self,
        symbol: &str,
        bid_price: Decimal,
        bid_quantity: Decimal,
        ask_price: Decimal,
        ask_quantity: Decimal,
    ) -> Result<(), ()> {
        self.require_connected().map_err(|_| ())?;
        let fields = codec::build_mass_quote(symbol, bid_price, bid_quantity, ask_price, ask_quantity);
        self.cmd_tx.send(OutMsg::Message { msg_type: msg_type::MASS_QUOTE, fields }).map_err(|_| ())
    }
}

fn handle_execution_report(shared: &InitiatorShared, data: crate::fix::codec::ExecReportData) {
    let is_quote = data.exchange_id.starts_with("quote.");
    let id = if is_quote { 0 } else { data.cl_ord_id };

    let mut order_view: Option<OrderView> = None;
    if !is_quote {
        let mut orders = shared.orders.lock().unwrap();
        if let Some(record) = orders.get_mut(&id) {
            record.exchange_id = data.exchange_id.clone();
            record.remaining = data.remaining;
            record.price = data.price;
            record.quantity = data.quantity;
            record.state = data.state;
            order_view = Some(record.clone());
        } else {
            log::warn!("unknown order clOrdID {}", data.cl_ord_id);
        }
    }

    if data.is_fill {
        let fill = FillView {
            is_quote,
            order_id: if is_quote { None } else { Some(id) },
            exchange_id: data.exchange_id.clone(),
            symbol: data.symbol.clone(),
            side: data.side,
            price: data.last_price,
            quantity: data.last_quantity,
        };
        shared.callback.lock().unwrap().on_fill(&fill);
    }

    if let Some(view) = &order_view {
        shared.callback.lock().unwrap().on_order_status(view);
    }
}
