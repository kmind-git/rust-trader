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
use super::log::{LogConfig, SessionLog};

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
    log: SessionLog,
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
                state.log.event(&format!("Sent SequenceReset TO: {}", this_seq + 1));
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
        state.log.outgoing(&String::from_utf8_lossy(&bytes));
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
    pub log: LogConfig,
    /// engine-level admission (ADR-0006): quickfix's default model only
    /// accepts declared sessions; DynamicSessions=Y restores accept-all
    pub admission: Admission,
}

/// which client CompIDs may log on
#[derive(Clone, Debug)]
pub enum Admission {
    /// any CompID (go-trader's DynamicSessions=Y behavior)
    Dynamic,
    /// only these client CompIDs (the declared session table)
    Declared(Vec<String>),
}

fn client_admitted(admission: &Admission, client_comp_id: &str) -> bool {
    match admission {
        Admission::Dynamic => true,
        Admission::Declared(targets) => targets.iter().any(|t| t == client_comp_id),
    }
}

/// quickfixgo peerTimer semantics (session.go:567): probe the peer after
/// 1.2x HeartBtInt of silence, disconnect after 2.4x
fn peer_timeouts(heart_bt_int: u32) -> (Duration, Duration) {
    let heart = Duration::from_secs(heart_bt_int.max(1) as u64);
    (heart.mul_f64(1.2), heart.mul_f64(2.4))
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
    let mut reader = BufReader::new(stream.try_clone()?);
    // silence is measured in the loop (1.2x/2.4x HeartBtInt), so the socket
    // read timeout is just a coarse poll tick
    reader.get_ref().set_read_timeout(Some(Duration::from_millis(500)))?;
    let mut writer_stream = stream.try_clone()?;

    // logon handshake: first message must be a Logon addressed to us
    let logon = match frame::read_message(&mut reader)? {
        Some(message) => message,
        None => return Ok(()),
    };
    let client_comp_id = match logon.get(49) {
        Some(v) => v.to_string(),
        None => {
            log::warn!("logon without SenderCompID (49), closing");
            return Ok(());
        }
    };
    // engine-level admission: undeclared clients are rejected before any
    // session state (or log file) is created for them
    if !client_admitted(&cfg.admission, &client_comp_id) {
        log::warn!("FIX logon from undeclared client {}, closing", client_comp_id);
        return Ok(());
    }
    // per-session FIX logs start with the first received message
    let session_log = if cfg.log.enabled {
        match SessionLog::new(&cfg.log.dir, &cfg.begin_string, &cfg.sender_comp_id, &client_comp_id) {
            Ok(log) => log,
            Err(e) => {
                log::warn!("unable to create FIX logs in {}: {}", cfg.log.dir, e);
                SessionLog::disabled()
            }
        }
    } else {
        SessionLog::disabled()
    };
    session_log.incoming(&logon.raw);
    session_log.event("Received logon request");
    if logon.msg_type() != Some(msg_type::LOGON) {
        log::warn!("first message was not a Logon, closing");
        session_log.event("Failed handshake: first message was not a Logon");
        return Ok(());
    }
    if logon.begin_string != cfg.begin_string {
        log::warn!("unsupported FixVersion {}, closing", logon.begin_string);
        session_log.event(&format!("Failed handshake: unsupported FixVersion {}", logon.begin_string));
        return Ok(());
    }
    let target = match logon.get(56) {
        Some(t) => t,
        None => {
            log::warn!("logon without TargetCompID (56), closing");
            session_log.event("Failed handshake: logon without TargetCompID (56)");
            return Ok(());
        }
    };
    if target != cfg.sender_comp_id {
        log::warn!("logon for unknown target {}, closing", target);
        session_log.event(&format!("Failed handshake: logon for unknown target {}", target));
        return Ok(());
    }
    let heart_bt_int: u32 = logon.get(codec::tags::HEART_BT_INT).and_then(|v| v.parse().ok()).unwrap_or(30);
    let (probe_after, close_after) = peer_timeouts(heart_bt_int);
    let heart = Duration::from_secs(heart_bt_int.max(1) as u64);

    let session_id = format!("{}:{}->{}", cfg.begin_string, cfg.sender_comp_id, client_comp_id);
    let (tx, rx) = mpsc::channel::<OutMsg>();
    let (report_tx, report_rx) = mpsc::channel::<Report>();
    engine.lock().unwrap().register_session(&session_id, report_tx);
    session_log.event(&format!("Created session {}", session_id));
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
    let writer_log = session_log.clone();
    let writer = std::thread::Builder::new()
        .name(format!("fix-writer-{}", client_comp_id))
        .spawn(move || {
            let state = WriterState {
                stream: writer_stream,
                begin_string,
                sender_comp_id,
                target_comp_id: client_comp_id,
                seq: 0,
                log: writer_log,
            };
            writer_loop(state, rx, heart.max(Duration::from_secs(5)))
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    // logon acknowledgement
    session_log.event("Responding to logon request");
    tx.send(OutMsg::Message {
        msg_type: msg_type::LOGON,
        fields: codec::build_logon(heart_bt_int),
    })
    .ok();
    log::info!("session {} logged on", session_id);

    let mut expected_in: u64 = 2; // the logon consumed sequence 1
    let mut last_received = std::time::Instant::now();
    let mut probed = false;
    loop {
        match frame::read_message(&mut reader) {
            Ok(None) => {
                log::warn!("session {}: peer closed connection", session_id);
                session_log.event("Connection Terminated");
                break;
            }
            Ok(Some(msg)) => {
                session_log.incoming(&msg.raw);
                last_received = std::time::Instant::now();
                probed = false;
                if msg.begin_string != cfg.begin_string {
                    session_log.event(&format!("Discarded message with BeginString {}", msg.begin_string));
                    continue;
                }
                match msg.seq() {
                    Some(s) if s < expected_in => {
                        log::warn!("session {}: duplicate seq {}, expected {}", session_id, s, expected_in);
                        session_log.event(&format!("MsgSeqNum too low, expecting {} but received {}", expected_in, s));
                        continue;
                    }
                    Some(s) if s > expected_in => {
                        log::warn!(
                            "session {}: seq gap (got {}, expected {}), requesting resend",
                            session_id, s, expected_in
                        );
                        session_log.event(&format!("MsgSeqNum too high, expecting {} but received {}", expected_in, s));
                        tx.send(OutMsg::Message {
                            msg_type: msg_type::RESEND_REQUEST,
                            fields: vec![(7, expected_in.to_string()), (16, "0".to_string())],
                        })
                        .ok();
                        session_log.event(&format!("Sent ResendRequest FROM: {} TO: {}", expected_in, 0));
                        continue;
                    }
                    _ => {}
                }
                expected_in += 1;

                let decoded = match codec::decode(&msg) {
                    Ok(decoded) => decoded,
                    Err(e) => {
                        log::warn!("session {}: bad message: {}", session_id, e);
                        session_log.event(&format!("Msg Parse Error: {}", e));
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
                        session_log.event("Received ResendRequest");
                        tx.send(OutMsg::GapFill).ok();
                    }
                    Inbound::Logout => {
                        session_log.event("Received logout request");
                        tx.send(OutMsg::Message { msg_type: msg_type::LOGOUT, fields: vec![] }).ok();
                        session_log.event("Sending logout response");
                        break;
                    }
                    Inbound::Logon { .. } => {
                        log::warn!("session {}: duplicate logon ignored", session_id);
                        session_log.event("Received duplicate logon request");
                    }
                    Inbound::Unsupported(t) => {
                        log::warn!("session {}: unsupported msg type {}", session_id, t);
                        session_log.event(&format!("Unsupported message type {}", t));
                    }
                    inbound => {
                        let mut engine = engine.lock().unwrap();
                        handle_business(&mut engine, &session_id, &tx, inbound);
                    }
                }
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut {
                    let silent = last_received.elapsed();
                    if silent >= close_after {
                        log::warn!("session {}: no data for {}s, closing", session_id, silent.as_secs());
                        session_log.event("Session Timeout");
                        break;
                    }
                    if silent >= probe_after && !probed {
                        tx.send(OutMsg::Message {
                            msg_type: msg_type::TEST_REQUEST,
                            fields: codec::build_test_request("ARE-YOU-THERE"),
                        })
                        .ok();
                        session_log.event("Sent test request");
                        probed = true;
                    }
                    continue;
                }
                log::warn!("session {}: read error: {} (kind {:?})", session_id, e, e.kind());
                session_log.event(&format!("Connection Terminated: {}", e));
                break;
            }
        }
    }
    engine.lock().unwrap().session_disconnect(&session_id);
    session_log.event("Disconnected");
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
        Inbound::CancelRequest { cl_ord_id, orig_cl_ord_id } => {
            // spec-style clients send a fresh ClOrdID (11) and point OrigClOrdID
            // (41) at the order; Go-style clients reuse the order id in 11
            let result = engine.cancel_order(session_id, cl_ord_id).or_else(|e| {
                if orig_cl_ord_id != cl_ord_id {
                    engine.cancel_order(session_id, orig_cl_ord_id)
                } else {
                    Err(e)
                }
            });
            if let Err(e) = result {
                log::warn!("cancel order failed: {}", e);
            }
        }
        Inbound::CancelReplace { cl_ord_id, orig_cl_ord_id, price, quantity } => {
            if let Err(e) = engine.modify_order(session_id, orig_cl_ord_id, cl_ord_id, price, quantity) {
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
    log: SessionLog,
}

pub struct InitiatorConfig {
    pub sender_comp_id: String,
    pub target_comp_id: String,
    pub host: String,
    pub port: u16,
    pub heart_bt_int: u32,
    pub log: LogConfig,
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
        let session_log = if cfg.log.enabled {
            match SessionLog::new(&cfg.log.dir, "FIX.4.2", &cfg.sender_comp_id, &cfg.target_comp_id) {
                Ok(log) => log,
                Err(e) => {
                    log::warn!("unable to create FIX logs in {}: {}", cfg.log.dir, e);
                    SessionLog::disabled()
                }
            }
        } else {
            SessionLog::disabled()
        };
        session_log.event(&format!("Connecting to: {}:{}", cfg.host, cfg.port));
        let stream = match TcpStream::connect((cfg.host.as_str(), cfg.port)) {
            Ok(stream) => stream,
            Err(e) => {
                session_log.event(&format!("Failed to connect: {}", e));
                return Err(ConnectError::Io(e));
            }
        };
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
            log: session_log.clone(),
        });

        // writer thread
        {
            let state = WriterState {
                stream: writer_stream,
                begin_string: "FIX.4.2".to_string(),
                sender_comp_id: cfg.sender_comp_id.clone(),
                target_comp_id: cfg.target_comp_id.clone(),
                seq: 0,
                log: session_log.clone(),
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
                            Ok(None) => {
                                shared.log.event("Connection Terminated");
                                break;
                            }
                            Err(_) => {
                                shared.log.event("Session Timeout");
                                break;
                            }
                        };
                        shared.log.incoming(&message.raw);
                        if message.get(56).map(|v| v != expected_target).unwrap_or(false) {
                            continue;
                        }
                        match message.seq() {
                            Some(s) if s < expected_in => {
                                log::warn!("duplicate seq {}", s);
                                shared.log.event(&format!("MsgSeqNum too low, expecting {} but received {}", expected_in, s));
                                continue;
                            }
                            Some(s) if s > expected_in => {
                                log::warn!("seq gap (got {}, expected {})", s, expected_in);
                                shared.log.event(&format!("MsgSeqNum too high, expecting {} but received {} (processing anyway)", expected_in, s));
                                // process anyway: as a client we can't afford to stall
                            }
                            _ => {}
                        }
                        expected_in = message.seq().map(|s| s + 1).unwrap_or(expected_in);
                        let decoded = match codec::decode(&message) {
                            Ok(decoded) => decoded,
                            Err(e) => {
                                log::warn!("bad message: {}", e);
                                shared.log.event(&format!("Msg Parse Error: {}", e));
                                continue;
                            }
                        };
                        match decoded {
                            Inbound::Logon { .. } => {
                                shared.log.event("Received logon response");
                                shared.log.event("In session");
                                shared.logged_in.set();
                            }
                            Inbound::Logout => {
                                shared.log.event("Received logout request");
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
                    shared.log.event("Disconnected");
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
        session_log.event("Sending logon request");
        initiator
            .cmd_tx
            .send(OutMsg::Message { msg_type: msg_type::LOGON, fields: codec::build_logon(cfg.heart_bt_int) })
            .map_err(|_| ConnectError::Timeout)?;
        if !initiator.shared.logged_in.wait_for(Duration::from_secs(30)) {
            session_log.event("Timed out waiting for logon response");
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
            self.shared.log.event("Initiated logout request");
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
        let (symbol, side) = {
            let orders = self.shared.orders.lock().unwrap();
            let record = orders.get(&id).ok_or(())?;
            (record.symbol.clone(), record.side)
        };
        // the cancel-replace request carries a fresh ClOrdID which becomes the
        // replacement order's id; track it alongside the original (which will
        // receive a Cancelled report)
        let req_id = {
            let mut next_order = self.next_order.lock().unwrap();
            *next_order += 1;
            *next_order
        };
        {
            let mut orders = self.shared.orders.lock().unwrap();
            orders.insert(
                req_id,
                OrderView {
                    id: req_id,
                    exchange_id: String::new(),
                    symbol: symbol.clone(),
                    side,
                    price,
                    quantity,
                    remaining: quantity,
                    state: OrderState::New,
                },
            );
        }
        let fields =
            codec::build_cancel_replace(&id.to_string(), &req_id.to_string(), &symbol, side, price, quantity);
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
        // fresh ClOrdID for the cancel request; OrigClOrdID points at the order
        let req_id = {
            let mut next_order = self.next_order.lock().unwrap();
            *next_order += 1;
            *next_order
        };
        let fields = codec::build_cancel_request(&id.to_string(), &req_id.to_string(), &record.symbol, record.side);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_admitted() {
        let dynamic = Admission::Dynamic;
        assert!(client_admitted(&dynamic, "ANYONE"));

        let declared = Admission::Declared(vec!["CLIENT".to_string(), "PLAYBACK".to_string()]);
        assert!(client_admitted(&declared, "CLIENT"));
        assert!(client_admitted(&declared, "PLAYBACK"));
        assert!(!client_admitted(&declared, "INTRUDER"));
        assert!(!client_admitted(&declared, ""));
    }

    #[test]
    fn test_peer_timeouts() {
        let (probe, close) = peer_timeouts(30);
        assert_eq!(probe, Duration::from_secs(36));
        assert_eq!(close, Duration::from_secs(72));
        // minimal heartbeat value stays sane
        let (probe, close) = peer_timeouts(1);
        assert_eq!(probe, Duration::from_millis(1200));
        assert_eq!(close, Duration::from_millis(2400));
    }
}
