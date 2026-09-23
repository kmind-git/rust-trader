//! FIX session layer. The acceptor (exchange side) and initiator (client
//! side) share the same framing and writer-thread design: the writer owns the
//! outbound sequence number, the reader validates inbound sequence numbers and
//! dispatches decoded messages.

use crate::queue::{self as mpsc, Sender};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use rust_decimal::Decimal;

use crate::core::exchange::{Engine, NewOrder, Report, ReportSink};
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
#[derive(Debug)]
pub enum OutMsg {
    Report(Report),
    Message {
        msg_type: &'static str,
        fields: Vec<(u32, String)>,
    },
    /// sequence reset answering a ResendRequest: we persist nothing, so we
    /// tell the counterparty to jump over the requested range
    GapFill {
        begin_seq: u64,
        end_seq: u64,
    },
    /// Acceptor's final Logout reply/error: send it after already queued
    /// reports, then close without generating any later heartbeat/message.
    LogoutAndShutdown {
        fields: Vec<(u32, String)>,
    },
    Shutdown,
}

/// Move engine reports directly into the same bounded mailbox as session
/// messages. There is no intermediate queue or reader-thread report pump.
struct FixReportSink(Sender<OutMsg>);

impl ReportSink for FixReportSink {
    fn try_send(&self, report: Report) -> Result<(), TrySendError<Report>> {
        self.0
            .send(OutMsg::Report(report))
            .map_err(|error| match error {
                TrySendError::Full(OutMsg::Report(report)) => TrySendError::Full(report),
                TrySendError::Disconnected(OutMsg::Report(report)) => {
                    TrySendError::Disconnected(report)
                }
                _ => unreachable!("the rejected value is the report just submitted"),
            })
    }
}

struct WriterState {
    stream: TcpStream,
    begin_string: String,
    sender_comp_id: String,
    target_comp_id: String,
    seq: u64,
    log: SessionLog,
    wire_ids: Option<Arc<Mutex<WireIdMap>>>,
}

/// FIX ClOrdID is a STRING. The matching engine currently uses an i32 key, so
/// each live connection keeps a lossless wire-to-engine mapping. Numeric IDs
/// retain their value when possible for compatibility with the existing API.
#[derive(Default)]
struct WireIdMap {
    wire_to_internal: HashMap<String, OrderId>,
    internal_to_wire: HashMap<OrderId, String>,
    next_internal: OrderId,
}

impl WireIdMap {
    fn new() -> Self {
        Self {
            next_internal: 1,
            ..Self::default()
        }
    }

    fn intern(&mut self, wire: &str) -> Result<OrderId, String> {
        if wire.is_empty() {
            return Err("ClOrdID must not be empty".to_string());
        }
        if let Some(id) = self.wire_to_internal.get(wire) {
            return Ok(*id);
        }
        let preferred = wire
            .parse::<OrderId>()
            .ok()
            .filter(|id| *id > 0 && *id != crate::core::exchange::QUOTE_ORDER_ID);
        let id = match preferred {
            Some(id) if !self.internal_to_wire.contains_key(&id) => id,
            _ => {
                let start = self.next_internal.max(1);
                let mut candidate = start;
                loop {
                    if candidate > 0
                        && candidate != crate::core::exchange::QUOTE_ORDER_ID
                        && !self.internal_to_wire.contains_key(&candidate)
                    {
                        self.next_internal = candidate.saturating_add(1).max(1);
                        break candidate;
                    }
                    candidate = candidate
                        .checked_add(1)
                        .ok_or_else(|| "too many ClOrdIDs".to_string())?;
                }
            }
        };
        self.wire_to_internal.insert(wire.to_string(), id);
        self.internal_to_wire.insert(id, wire.to_string());
        Ok(id)
    }

    fn wire_for(&self, id: OrderId) -> Option<String> {
        self.internal_to_wire.get(&id).cloned()
    }
}

fn report_wire_id(report: &Report, ids: &Arc<Mutex<WireIdMap>>) -> Option<String> {
    // A status generated for a cancel/replace belongs to the request's fresh
    // ClOrdID, which is deliberately different from the order snapshot ID.
    let order_id = match report {
        Report::Status { cl_ord_id, .. } => *cl_ord_id,
        Report::Fill { order, .. } => order.id,
    };
    ids.lock().ok().and_then(|map| map.wire_for(order_id))
}

/// `PossDupFlag(43)` and `OrigSendingTime(122)` are standard-header fields,
/// while `frame::frame` intentionally exposes only the common header. GapFill
/// is the one outbound message that needs them, so encode that header locally
/// and keep all ordinary messages on the shared frame implementation.
fn frame_gap_fill(
    begin_string: &str,
    msg_type: &str,
    seq: u64,
    sender: &str,
    target: &str,
    fields: &[(u32, String)],
) -> Vec<u8> {
    let mut body = String::new();
    body.push_str(&format!("35={}\x01", msg_type));
    body.push_str(&format!("34={}\x01", seq));
    body.push_str(&format!("49={}\x01", sender));
    body.push_str(&format!("56={}\x01", target));
    body.push_str("43=Y\x01");
    let timestamp = frame::utc_timestamp();
    body.push_str(&format!("52={}\x01", timestamp));
    body.push_str(&format!("122={}\x01", timestamp));
    for (tag, value) in fields {
        body.push_str(&format!("{}={}\x01", tag, value));
    }
    let mut message = format!("8={}\x019={}\x01", begin_string, body.len());
    message.push_str(&body);
    let checksum: u32 = message.bytes().map(u32::from).sum::<u32>() % 256;
    message.push_str(&format!("10={:03}\x01", checksum));
    message.into_bytes()
}

fn writer_loop(
    mut state: WriterState,
    rx: Receiver<OutMsg>,
    idle: Duration,
    failed: Arc<std::sync::atomic::AtomicBool>,
) {
    loop {
        if failed.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        let out = match rx.recv_timeout(idle) {
            Ok(out) => out,
            Err(RecvTimeoutError::Timeout) => OutMsg::Message {
                msg_type: msg_type::HEARTBEAT,
                fields: vec![],
            },
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let (out, close_after_write) = match out {
            OutMsg::LogoutAndShutdown { fields } => (
                OutMsg::Message {
                    msg_type: "5",
                    fields,
                },
                true,
            ),
            other => (other, false),
        };
        let bytes = match out {
            OutMsg::Shutdown => break,
            OutMsg::LogoutAndShutdown { .. } => unreachable!("normalized above"),
            OutMsg::GapFill { begin_seq, end_seq } => {
                let begin_seq = begin_seq.max(1);
                let effective_end = if end_seq == 0 {
                    state.seq
                } else {
                    end_seq.min(state.seq)
                };
                if begin_seq > effective_end {
                    continue;
                }
                let new_seq = effective_end.saturating_add(1);
                state.log.event(&format!(
                    "Sent SequenceReset FROM: {} TO: {}",
                    begin_seq, new_seq
                ));
                state.seq = state.seq.max(effective_end);
                frame_gap_fill(
                    &state.begin_string,
                    msg_type::SEQUENCE_RESET,
                    begin_seq,
                    &state.sender_comp_id,
                    &state.target_comp_id,
                    &[
                        (codec::tags::NEW_SEQ_NO, new_seq.to_string()),
                        (codec::tags::GAP_FILL_FLAG, "Y".to_string()),
                    ],
                )
            }
            OutMsg::Message { msg_type, fields } => {
                state.seq = state.seq.saturating_add(1);
                frame::frame(
                    &state.begin_string,
                    msg_type,
                    state.seq,
                    &state.sender_comp_id,
                    &state.target_comp_id,
                    &fields,
                )
            }
            OutMsg::Report(report) => {
                state.seq = state.seq.saturating_add(1);
                let mut fields = codec::build_execution_report(&report);
                if let Some(ids) = &state.wire_ids {
                    for (tag, value) in &mut fields {
                        if *tag == 41 {
                            if let Ok(id) = value.parse::<i32>() {
                                if let Some(wire) = ids.lock().unwrap().wire_for(id) {
                                    *value = wire;
                                }
                            }
                        }
                    }
                    if let Some(wire_id) = report_wire_id(&report, ids) {
                        if let Some((_, value)) = fields
                            .iter_mut()
                            .find(|(tag, _)| *tag == codec::tags::CL_ORD_ID)
                        {
                            *value = wire_id;
                        }
                    }
                }
                frame::frame(
                    &state.begin_string,
                    msg_type::EXECUTION_REPORT,
                    state.seq,
                    &state.sender_comp_id,
                    &state.target_comp_id,
                    &fields,
                )
            }
        };
        state.log.outgoing(&String::from_utf8_lossy(&bytes));
        if state.stream.write_all(&bytes).is_err() || state.stream.flush().is_err() {
            break;
        }
        if close_after_write {
            break;
        }
    }
    let _ = state.stream.shutdown(std::net::Shutdown::Both);
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
    pub session_logs: HashMap<String, LogConfig>,
}

/// which client CompIDs may log on
#[derive(Clone, Debug)]
pub enum Admission {
    /// any CompID (DynamicSessions=Y)
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

const SUPPORTED_BEGIN_STRING: &str = "FIX.4.2";
const LOGON_READ_TIMEOUT: Duration = Duration::from_secs(10);
const SOCKET_POLL: Duration = Duration::from_millis(500);
const MAX_QUEUED_MESSAGES: usize = 4096;

#[derive(Default)]
struct SequenceState {
    expected: u64,
    queued: BTreeMap<u64, frame::FixMessage>,
    resend_from: Option<u64>,
}

impl SequenceState {
    fn new(expected: u64) -> Self {
        Self {
            expected,
            ..Self::default()
        }
    }

    fn is_duplicate(msg: &frame::FixMessage) -> bool {
        matches!(msg.get(43), Some("Y"))
    }

    /// Validate and classify an incoming message. High sequence messages are
    /// retained until the missing range is recovered; low sequence duplicates
    /// are accepted only when PossDupFlag=Y and are never replayed to the
    /// application.
    fn classify(&mut self, msg: frame::FixMessage) -> Result<SequenceAction, String> {
        let seq = parse_required_seq(&msg)?;
        if msg.msg_type() == Some(msg_type::SEQUENCE_RESET) {
            let new_seq = parse_required_u64(&msg, 36)?;
            if msg.get(123) != Some("Y")
                || (seq < self.expected && new_seq > self.expected && Self::is_duplicate(&msg))
            {
                return Ok(SequenceAction::Ready(msg));
            }
        }
        if seq < self.expected {
            if Self::is_duplicate(&msg) {
                return Ok(SequenceAction::Duplicate);
            }
            return Err(format!(
                "MsgSeqNum too low, expecting {} but received {}",
                self.expected, seq
            ));
        }
        if seq > self.expected {
            if self.queued.len() >= MAX_QUEUED_MESSAGES {
                return Err("too many queued messages while recovering sequence gap".to_string());
            }
            self.queued.entry(seq).or_insert(msg);
            let request = if self.resend_from.is_none() {
                self.resend_from = Some(self.expected);
                Some(self.expected)
            } else {
                None
            };
            return Ok(SequenceAction::Gap {
                request_from: request,
            });
        }
        Ok(SequenceAction::Ready(msg))
    }

    /// Advance the inbound sequence for a message whose sequence is exactly
    /// `expected`. SequenceReset-GapFill jumps directly to NewSeqNo and does
    /// not consume the sequence range one-by-one.
    fn advance(&mut self, msg: &frame::FixMessage) -> Result<(), String> {
        let seq = parse_required_seq(msg)?;
        if seq != self.expected && msg.msg_type() != Some(msg_type::SEQUENCE_RESET) {
            return Err(format!(
                "internal sequence mismatch: expected {}, got {}",
                self.expected, seq
            ));
        }
        if msg.msg_type() == Some(msg_type::SEQUENCE_RESET) {
            let new_seq = parse_required_u64(msg, codec::tags::NEW_SEQ_NO)?;
            if new_seq <= self.expected {
                return Err(format!(
                    "SequenceReset NewSeqNo {} is not above {}",
                    new_seq, self.expected
                ));
            }
            self.expected = new_seq;
            let old = std::mem::take(&mut self.queued);
            self.queued = old.into_iter().filter(|(n, _)| *n >= new_seq).collect();
        } else {
            self.expected = self.expected.saturating_add(1);
        }
        if self.resend_from.is_some_and(|from| self.expected > from) {
            self.resend_from = None;
        }
        Ok(())
    }

    fn take_ready(&mut self) -> Option<frame::FixMessage> {
        self.queued.remove(&self.expected)
    }
}

enum SequenceAction {
    Ready(frame::FixMessage),
    Gap { request_from: Option<u64> },
    Duplicate,
}

fn parse_required_u64(msg: &frame::FixMessage, tag: u32) -> Result<u64, String> {
    let value = msg
        .get(tag)
        .ok_or_else(|| format!("missing required tag {}", tag))?;
    let parsed = value
        .parse::<u64>()
        .map_err(|_| format!("tag {} is not a valid positive integer", tag))?;
    if parsed == 0 {
        return Err(format!("tag {} must be positive", tag));
    }
    Ok(parsed)
}

fn parse_required_seq(msg: &frame::FixMessage) -> Result<u64, String> {
    parse_required_u64(msg, 34)
}

fn parse_required_text(msg: &frame::FixMessage, tag: u32) -> Result<&str, String> {
    let value = msg
        .get(tag)
        .ok_or_else(|| format!("missing required tag {}", tag))?;
    if value.is_empty() {
        return Err(format!("tag {} must not be empty", tag));
    }
    Ok(value)
}

fn parse_fix_timestamp(value: &str, tag: u32) -> Result<chrono::NaiveDateTime, String> {
    let format = match value.len() {
        17 => "%Y%m%d-%H:%M:%S",
        21 => "%Y%m%d-%H:%M:%S%.3f",
        _ => return Err(format!("tag {} has invalid FIX timestamp length", tag)),
    };
    chrono::NaiveDateTime::parse_from_str(value, format)
        .map_err(|_| format!("tag {} is not a valid FIX timestamp", tag))
}

fn validate_timestamp_fields(msg: &frame::FixMessage) -> Result<(), String> {
    let sending_time = parse_fix_timestamp(parse_required_text(msg, 52)?, 52)?;
    match msg.get(43) {
        None | Some("N") => {}
        Some("Y") => {
            let original = parse_fix_timestamp(parse_required_text(msg, 122)?, 122)?;
            if original > sending_time {
                return Err("OrigSendingTime(122) is after SendingTime(52)".to_string());
            }
        }
        Some(other) => return Err(format!("PossDupFlag(43) must be Y or N, got {}", other)),
    }
    Ok(())
}

fn validate_unique_header_tag(msg: &frame::FixMessage, tag: u32) -> Result<&str, String> {
    let mut values = msg
        .fields
        .iter()
        .filter(|(field, _)| *field == tag)
        .map(|(_, value)| value.as_str());
    let value = values
        .next()
        .ok_or_else(|| format!("missing required tag {}", tag))?;
    if values.next().is_some() {
        return Err(format!("duplicate required header tag {}", tag));
    }
    if value.is_empty() {
        return Err(format!("tag {} must not be empty", tag));
    }
    Ok(value)
}

fn validate_header(
    msg: &frame::FixMessage,
    begin_string: &str,
    expected_sender: &str,
    expected_target: &str,
) -> Result<u64, String> {
    if msg.begin_string != begin_string {
        return Err(format!("unsupported BeginString {}", msg.begin_string));
    }
    let _ = validate_unique_header_tag(msg, 35)?;
    let sender = validate_unique_header_tag(msg, 49)?;
    let target = validate_unique_header_tag(msg, 56)?;
    if sender != expected_sender || target != expected_target {
        return Err(format!(
            "CompID mismatch: expected {} -> {}, received {} -> {}",
            expected_sender, expected_target, sender, target
        ));
    }
    let _ = validate_unique_header_tag(msg, 34)?;
    let _ = validate_unique_header_tag(msg, 52)?;
    validate_timestamp_fields(msg)?;
    parse_required_seq(msg)
}

fn validate_logon(
    msg: &frame::FixMessage,
    cfg: &AcceptorConfig,
) -> Result<(String, u32, u64), String> {
    if msg.begin_string != cfg.begin_string {
        return Err(format!("unsupported BeginString {}", msg.begin_string));
    }
    if msg.msg_type() != Some(msg_type::LOGON) {
        return Err("first message was not a Logon".to_string());
    }
    let sender = validate_unique_header_tag(msg, 49)?.to_string();
    let target = validate_unique_header_tag(msg, 56)?;
    if target != cfg.sender_comp_id {
        return Err(format!("logon for unknown target {}", target));
    }
    let _ = validate_unique_header_tag(msg, 34)?;
    let _ = validate_unique_header_tag(msg, 52)?;
    validate_timestamp_fields(msg)?;
    if parse_required_text(msg, codec::tags::ENCRYPT_METHOD)? != "0" {
        return Err("only EncryptMethod=0 is supported".to_string());
    }
    let heart = parse_required_u64(msg, codec::tags::HEART_BT_INT)?;
    if heart > u32::MAX as u64 {
        return Err("HeartBtInt is too large".to_string());
    }
    let seq = parse_required_seq(msg)?;
    Ok((sender, heart as u32, seq))
}

fn parse_resend_range(msg: &frame::FixMessage) -> Result<(u64, u64), String> {
    let begin = parse_required_u64(msg, 7)?;
    let end = msg
        .get(16)
        .ok_or_else(|| "missing required tag 16".to_string())?
        .parse::<u64>()
        .map_err(|_| "tag 16 is not a valid sequence number".to_string())?;
    if end != 0 && end < begin {
        return Err(format!("EndSeqNo {} is below BeginSeqNo {}", end, begin));
    }
    Ok((begin, end))
}

struct SessionReservation {
    active: Arc<Mutex<HashSet<String>>>,
    id: String,
}

impl Drop for SessionReservation {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(&self.id);
        }
    }
}

fn valid_component_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.is_ascii()
        && value
            .bytes()
            .all(|byte| (0x21..=0x7e).contains(&byte) && byte != b'=')
}

fn validate_acceptor_config(cfg: &AcceptorConfig) -> std::io::Result<()> {
    if cfg.begin_string != SUPPORTED_BEGIN_STRING {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("only {} is supported", SUPPORTED_BEGIN_STRING),
        ));
    }
    if cfg.port == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "acceptor port must be nonzero",
        ));
    }
    if !valid_component_id(&cfg.sender_comp_id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid acceptor SenderCompID",
        ));
    }
    if let Admission::Declared(targets) = &cfg.admission {
        if targets.is_empty() || targets.iter().any(|target| !valid_component_id(target)) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid declared TargetCompID",
            ));
        }
    }
    Ok(())
}

fn validate_initiator_config(cfg: &InitiatorConfig) -> Result<(), String> {
    if !valid_component_id(&cfg.sender_comp_id) || !valid_component_id(&cfg.target_comp_id) {
        return Err("SenderCompID and TargetCompID must be printable ASCII IDs".to_string());
    }
    if cfg.host.is_empty() || cfg.port == 0 {
        return Err("initiator host and port are required".to_string());
    }
    if cfg.heart_bt_int == 0 {
        return Err("HeartBtInt must be greater than zero".to_string());
    }
    Ok(())
}

fn send_session_reject(
    tx: &Sender<OutMsg>,
    seq: Option<u64>,
    rejected_type: Option<&str>,
    reason: u32,
    text: &str,
) {
    let mut fields = Vec::new();
    if let Some(seq) = seq {
        fields.push((45, seq.to_string()));
    }
    if let Some(msg_type) = rejected_type {
        fields.push((372, msg_type.to_string()));
    }
    fields.push((373, reason.to_string()));
    fields.push((58, text.to_string()));
    tx.send(OutMsg::Message {
        msg_type: msg_type::REJECT,
        fields,
    })
    .ok();
}

fn send_message_reject(tx: &Sender<OutMsg>, seq: Option<u64>, rejected_type: &str, text: &str) {
    send_session_reject(tx, seq, Some(rejected_type), 11, text);
}

fn send_session_logout(tx: &Sender<OutMsg>, text: &str) {
    tx.send(OutMsg::Message {
        msg_type: msg_type::LOGOUT,
        fields: vec![(codec::tags::TEXT, text.to_string())],
    })
    .ok();
}

fn send_business_reject(
    tx: &Sender<OutMsg>,
    seq: Option<u64>,
    rejected_type: &str,
    reason: u32,
    text: &str,
) {
    let mut fields = vec![
        (372, rejected_type.to_string()),
        (380, reason.to_string()),
        (58, text.to_string()),
    ];
    if let Some(seq) = seq {
        fields.insert(0, (45, seq.to_string()));
    }
    tx.send(OutMsg::Message {
        msg_type: msg_type::BUSINESS_MESSAGE_REJECT,
        fields,
    })
    .ok();
}

fn send_cancel_reject(
    tx: &Sender<OutMsg>,
    cl_ord_id: &str,
    orig_cl_ord_id: &str,
    response_to: &str,
    order: Option<&crate::core::order::Order>,
    reason: &str,
) {
    tx.send(OutMsg::Message {
        msg_type: msg_type::ORDER_CANCEL_REJECT,
        fields: vec![
            (
                37,
                order
                    .map(|o| o.exchange_id.clone())
                    .unwrap_or_else(|| "NONE".into()),
            ),
            (codec::tags::CL_ORD_ID, cl_ord_id.to_string()),
            (codec::tags::ORIG_CL_ORD_ID, orig_cl_ord_id.to_string()),
            (434, response_to.to_string()),
            (102, if order.is_some() { "0" } else { "1" }.to_string()),
            (
                codec::tags::ORD_STATUS,
                order
                    .map(|o| codec::ord_status_to_fix(o.state))
                    .unwrap_or("8")
                    .to_string(),
            ),
            (codec::tags::TEXT, reason.to_string()),
        ],
    })
    .ok();
}

fn rejected_execution_fields(
    cl_ord_id: &str,
    orig_cl_ord_id: Option<&str>,
    symbol: &str,
    side: Side,
    order_type: OrderType,
    quantity: Decimal,
    reason: &str,
) -> Vec<(u32, String)> {
    static NEXT_REJECT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let exec_id = format!(
        "REJ-{}",
        NEXT_REJECT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let mut fields = vec![
        (codec::tags::ORDER_ID, "0".to_string()),
        (codec::tags::EXEC_ID, exec_id),
        (codec::tags::EXEC_TRANS_TYPE, "0".to_string()),
        (codec::tags::EXEC_TYPE, "8".to_string()),
        (codec::tags::ORD_STATUS, "8".to_string()),
        (codec::tags::SIDE, codec::side_to_fix(side).to_string()),
        (codec::tags::ORDER_QTY, codec::fix_decimal(quantity)),
        (
            codec::tags::ORD_TYPE,
            if order_type == OrderType::Market {
                "1"
            } else {
                "2"
            }
            .to_string(),
        ),
        (codec::tags::LEAVES_QTY, "0".to_string()),
        (codec::tags::CUM_QTY, "0".to_string()),
        (codec::tags::PRICE, "0".to_string()),
        (codec::tags::CL_ORD_ID, cl_ord_id.to_string()),
        (codec::tags::SYMBOL, symbol.to_string()),
        (codec::tags::TEXT, reason.to_string()),
        (codec::tags::TRANSACT_TIME, frame::utc_timestamp()),
    ];
    if let Some(orig) = orig_cl_ord_id {
        fields.push((codec::tags::ORIG_CL_ORD_ID, orig.to_string()));
    }
    fields
}

/// blocking accept loop; spawn this on its own thread
pub fn run_acceptor(engine: Arc<Mutex<Engine>>, cfg: AcceptorConfig) -> std::io::Result<()> {
    validate_acceptor_config(&cfg)?;
    let listener = TcpListener::bind(("0.0.0.0", cfg.port))?;
    log::info!("FIX acceptor listening on port {}", cfg.port);
    let active_sessions = Arc::new(Mutex::new(HashSet::<String>::new()));
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let engine = Arc::clone(&engine);
                let cfg = cfg.clone();
                let active_sessions = Arc::clone(&active_sessions);
                std::thread::spawn(move || {
                    if let Err(e) = handle_acceptor_connection(stream, engine, cfg, active_sessions)
                    {
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
    mut stream: TcpStream,
    engine: Arc<Mutex<Engine>>,
    cfg: AcceptorConfig,
    active_sessions: Arc<Mutex<HashSet<String>>>,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(LOGON_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(LOGON_READ_TIMEOUT))?;
    let mut reader = frame::FrameReader::new(BufReader::new(stream.try_clone()?));
    let logon = match reader.read_message()? {
        Some(m) => m,
        None => return Ok(()),
    };
    let (client, heart, logon_seq) =
        match validate_logon(&logon, &cfg).and_then(|v| codec::decode(&logon).map(|_| v)) {
            Ok(v) => v,
            Err(e) => {
                if let Some(peer) = logon.get(49).filter(|s| valid_component_id(s)) {
                    let _ = stream.write_all(&frame::frame(
                        "FIX.4.2",
                        "5",
                        1,
                        &cfg.sender_comp_id,
                        peer,
                        &[(58, e)],
                    ));
                }
                return Ok(());
            }
        };
    let direct_logout = |stream: &mut TcpStream, reason: &str| {
        let _ = stream.write_all(&frame::frame(
            "FIX.4.2",
            "5",
            1,
            &cfg.sender_comp_id,
            &client,
            &[(58, reason.into())],
        ));
    };
    if !client_admitted(&cfg.admission, &client) {
        direct_logout(&mut stream, "session not declared");
        return Ok(());
    }
    if logon.get(141) == Some("Y") && logon_seq != 1 {
        direct_logout(&mut stream, "reset Logon must have sequence 1");
        return Ok(());
    }
    let session_id = format!("{}:{}->{}", cfg.begin_string, cfg.sender_comp_id, client);
    let _reservation = {
        let mut active = active_sessions.lock().unwrap();
        if !active.insert(session_id.clone()) {
            direct_logout(&mut stream, "session already active");
            return Ok(());
        }
        SessionReservation {
            active: Arc::clone(&active_sessions),
            id: session_id.clone(),
        }
    };
    let log_config = cfg.session_logs.get(&client).unwrap_or(&cfg.log);
    let session_log = if log_config.enabled {
        SessionLog::new(
            &log_config.dir,
            &cfg.begin_string,
            &cfg.sender_comp_id,
            &client,
        )?
    } else {
        SessionLog::disabled()
    };
    session_log.incoming(&logon.raw);
    session_log.event("Received logon request");
    // Set the timeout on the actual reading handle. Updating a previously
    // cloned control handle does not update SO_RCVTIMEO on all platforms.
    // Retain this BufReader across Logon so any prefetched next frame survives.
    reader
        .get_ref()
        .get_ref()
        .set_read_timeout(Some(SOCKET_POLL))?;
    let ids = Arc::new(Mutex::new(WireIdMap::new()));
    let (tx, rx) = mpsc::channel();
    let writer_state = WriterState {
        stream: stream.try_clone()?,
        begin_string: cfg.begin_string.clone(),
        sender_comp_id: cfg.sender_comp_id.clone(),
        target_comp_id: client.clone(),
        seq: 0,
        log: session_log.clone(),
        wire_ids: Some(ids.clone()),
    };
    let failed = tx.failure_flag();
    let writer = std::thread::spawn(move || {
        writer_loop(writer_state, rx, Duration::from_secs(heart as u64), failed)
    });
    let mut ack = codec::build_logon(heart);
    if logon.get(141) == Some("Y") {
        ack.push((141, "Y".into()));
    }
    let _ = tx.send(OutMsg::Message {
        msg_type: "A",
        fields: ack,
    });
    let symbols = engine.lock().unwrap().all_symbols();
    for symbol in &symbols {
        let id = engine
            .lock()
            .unwrap()
            .instrument_by_symbol(symbol)
            .unwrap()
            .id;
        let mut fields = codec::build_security_definition("bootstrap", symbol, id);
        fields.iter_mut().find(|(t, _)| *t == 393).unwrap().1 = symbols.len().to_string();
        let _ = tx.send(OutMsg::Message {
            msg_type: "d",
            fields,
        });
    }
    let mut sequence = SequenceState::new(if logon_seq == 1 { 2 } else { 1 });
    if logon_seq > 1 {
        sequence.queued.insert(logon_seq, logon);
        sequence.resend_from = Some(1);
        let _ = tx.send(OutMsg::Message {
            msg_type: "2",
            fields: vec![(7, "1".into()), (16, "0".into())],
        });
    }
    // Establish the Logon/bootstrap output order before exposing this session
    // as a report destination. All later application events use this mailbox.
    engine
        .lock()
        .unwrap()
        .register_session_sink(&session_id, FixReportSink(tx.clone()));
    let (probe_after, close_after) = peer_timeouts(heart);
    let mut last_received = std::time::Instant::now();
    let mut pending_test: Option<(String, std::time::Instant)> = None;
    let mut logout_fields = None;
    loop {
        if tx.failed() {
            session_log.event("Outbound queue overflow/disconnected; closing session");
            let _ = stream.shutdown(std::net::Shutdown::Both);
            break;
        }
        engine.lock().unwrap().expire_day_orders();
        if pending_test
            .as_ref()
            .is_some_and(|(_, deadline)| std::time::Instant::now() >= *deadline)
        {
            logout_fields = Some(vec![(58, "TestRequest was not answered".into())]);
            break;
        }
        let queued = sequence.take_ready();
        let from_queue = queued.is_some();
        let received = match queued {
            Some(m) => Ok(Some(m)),
            None => reader.read_message(),
        };
        let msg = match received {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if pending_test.is_none() && last_received.elapsed() >= probe_after {
                    let id = format!("probe-{}", sequence.expected);
                    let _ = tx.send(OutMsg::Message {
                        msg_type: "1",
                        fields: codec::build_test_request(&id),
                    });
                    pending_test = Some((id, std::time::Instant::now() + close_after));
                }
                continue;
            }
            Err(e) => {
                session_log.incoming_bytes(reader.buffered_bytes());
                session_log.event(&e.to_string());
                break;
            }
        };
        if !from_queue {
            session_log.incoming(&msg.raw);
            last_received = std::time::Instant::now();
        }
        if let Err(e) = validate_header(&msg, &cfg.begin_string, &client, &cfg.sender_comp_id) {
            logout_fields = Some(vec![(58, e)]);
            break;
        }
        let msg = match sequence.classify(msg) {
            Ok(SequenceAction::Ready(m)) => m,
            Ok(SequenceAction::Duplicate) => continue,
            Ok(SequenceAction::Gap { request_from }) => {
                if let Some(n) = request_from {
                    let _ = tx.send(OutMsg::Message {
                        msg_type: "2",
                        fields: vec![(7, n.to_string()), (16, "0".into())],
                    });
                }
                continue;
            }
            Err(e) => {
                logout_fields = Some(vec![(58, e)]);
                break;
            }
        };
        let decoded = codec::decode(&msg);
        // Invalid messages consume their sequence but must not modify the
        // recovery cursor using unvalidated SequenceReset fields.
        if decoded.is_err() {
            sequence.expected += 1;
            let reason = decoded.unwrap_err();
            send_session_reject(&tx, msg.seq(), msg.msg_type(), 5, &reason);
            continue;
        }
        if let Err(e) = sequence.advance(&msg) {
            logout_fields = Some(vec![(58, e)]);
            break;
        }
        if msg.msg_type() == Some("0")
            && pending_test
                .as_ref()
                .is_some_and(|(id, _)| msg.get(112) == Some(id))
        {
            pending_test = None;
        }
        match decoded.unwrap() {
            Inbound::Heartbeat | Inbound::SequenceReset { .. } => {}
            Inbound::TestRequest { test_req_id } => {
                let _ = tx.send(OutMsg::Message {
                    msg_type: "0",
                    fields: codec::build_heartbeat(Some(&test_req_id)),
                });
            }
            Inbound::ResendRequest => {
                let (begin_seq, end_seq) = parse_resend_range(&msg).unwrap();
                let _ = tx.send(OutMsg::GapFill { begin_seq, end_seq });
            }
            Inbound::Logout => {
                logout_fields = Some(vec![]);
                break;
            }
            Inbound::Logon { .. } if logon_seq > 1 && msg.seq() == Some(logon_seq) => {}
            Inbound::SessionReject { reason } | Inbound::BusinessReject { reason } => {
                session_log.event(&reason)
            }
            Inbound::Unsupported(kind) => {
                if matches!(kind.as_str(), "c" | "H" | "V" | "R" | "a" | "e" | "g") {
                    send_business_reject(
                        &tx,
                        msg.seq(),
                        &kind,
                        3,
                        "message not supported by this profile",
                    );
                } else {
                    send_message_reject(&tx, msg.seq(), &kind, "invalid or unsupported MsgType");
                }
            }
            inbound @ (Inbound::NewOrderSingle { .. }
            | Inbound::CancelRequest { .. }
            | Inbound::CancelReplace { .. }
            | Inbound::MassQuote { .. }) => {
                handle_business(
                    &mut engine.lock().unwrap(),
                    &session_id,
                    &tx,
                    inbound,
                    &ids,
                    msg.seq(),
                );
            }
            _ => send_business_reject(
                &tx,
                msg.seq(),
                msg.msg_type().unwrap(),
                3,
                "message not accepted in this direction",
            ),
        }
    }
    {
        let mut engine = engine.lock().unwrap();
        engine.session_disconnect(&session_id);
        // The same serialization boundary orders earlier committed reports
        // before Logout and prevents new reports targeting a closing session.
        if let Some(fields) = logout_fields {
            let _ = tx.send(OutMsg::LogoutAndShutdown { fields });
        } else {
            let _ = tx.send(OutMsg::Shutdown);
        }
    }
    let _ = writer.join();
    session_log.event("Disconnected");
    Ok(())
}

fn fresh_id(ids: &Arc<Mutex<WireIdMap>>, wire: &str) -> Result<i32, String> {
    let mut ids = ids.lock().unwrap();
    if ids.wire_to_internal.contains_key(wire) {
        return Err("duplicate ClOrdID".into());
    }
    ids.intern(wire)
}

fn handle_business(
    engine: &mut Engine,
    session: &str,
    tx: &Sender<OutMsg>,
    inbound: Inbound,
    ids: &Arc<Mutex<WireIdMap>>,
    _seq: Option<u64>,
) {
    match inbound {
        Inbound::NewOrderSingle {
            cl_ord_id,
            symbol,
            side,
            order_type,
            price,
            quantity,
        } => {
            let result = (|| {
                let id = fresh_id(ids, &cl_ord_id)?;
                let instrument_id = engine
                    .instrument_by_symbol(&symbol)
                    .ok_or("unknown symbol")?
                    .id;
                engine
                    .create_order(
                        session,
                        NewOrder {
                            id,
                            instrument_id,
                            side,
                            order_type,
                            price,
                            quantity,
                        },
                    )
                    .map_err(|e| e.to_string())?;
                Ok::<(), String>(())
            })();
            if let Err(e) = result {
                let mut fields = rejected_execution_fields(
                    &cl_ord_id, None, &symbol, side, order_type, quantity, &e,
                );
                fields.push((6, "0".into()));
                if let Some((_, v)) = fields.iter_mut().find(|(t, _)| *t == 44) {
                    *v = codec::fix_decimal(price);
                }
                let _ = tx.send(OutMsg::Message {
                    msg_type: "8",
                    fields,
                });
            }
        }
        Inbound::CancelRequest {
            cl_ord_id,
            orig_cl_ord_id,
            symbol,
            side,
        } => {
            let result = (|| {
                let request = fresh_id(ids, &cl_ord_id)?;
                let original = *ids
                    .lock()
                    .unwrap()
                    .wire_to_internal
                    .get(&orig_cl_ord_id)
                    .ok_or("unknown original ClOrdID")?;
                let order = engine.order(session, original).ok_or("unknown order")?;
                let instrument = engine
                    .instrument_by_symbol(&symbol)
                    .ok_or("unknown symbol")?;
                if order.side != side || order.instrument_id != instrument.id {
                    return Err("symbol/side differs from original order".into());
                }
                engine
                    .cancel_order_with_id(session, original, request)
                    .map_err(|e| e.to_string())
            })();
            if let Err(e) = result {
                send_cancel_reject(
                    tx,
                    &cl_ord_id,
                    &orig_cl_ord_id,
                    "1",
                    ids.lock()
                        .unwrap()
                        .wire_to_internal
                        .get(&orig_cl_ord_id)
                        .copied()
                        .and_then(|id| engine.order(session, id)),
                    &e,
                );
            }
        }
        Inbound::CancelReplace {
            cl_ord_id,
            orig_cl_ord_id,
            symbol,
            side,
            order_type,
            price,
            quantity,
        } => {
            let result = (|| {
                let request = fresh_id(ids, &cl_ord_id)?;
                let original = *ids
                    .lock()
                    .unwrap()
                    .wire_to_internal
                    .get(&orig_cl_ord_id)
                    .ok_or("unknown original ClOrdID")?;
                let order = engine.order(session, original).ok_or("unknown order")?;
                let instrument = engine
                    .instrument_by_symbol(&symbol)
                    .ok_or("unknown symbol")?;
                if order.side != side || order.instrument_id != instrument.id {
                    return Err("symbol/side differs from original order".into());
                }
                engine
                    .modify_order_with_type(session, original, request, order_type, price, quantity)
                    .map_err(|e| e.to_string())
            })();
            if let Err(e) = result {
                send_cancel_reject(
                    tx,
                    &cl_ord_id,
                    &orig_cl_ord_id,
                    "2",
                    ids.lock()
                        .unwrap()
                        .wire_to_internal
                        .get(&orig_cl_ord_id)
                        .copied()
                        .and_then(|id| engine.order(session, id)),
                    &e,
                );
            }
        }
        Inbound::MassQuote {
            quote_id,
            response_level,
            symbol,
            bid_px,
            bid_qty,
            offer_px,
            offer_qty,
        } => {
            let result = engine
                .instrument_by_symbol(&symbol)
                .map(|i| i.id)
                .ok_or_else(|| "unknown symbol".to_string())
                .and_then(|id| {
                    engine
                        .quote(session, id, bid_px, bid_qty, offer_px, offer_qty)
                        .map_err(|e| e.to_string())
                });
            if response_level == 2 || (response_level == 1 && result.is_err()) {
                let mut fields = codec::build_mass_quote_ack(&quote_id);
                if let Err(e) = result {
                    fields.iter_mut().find(|(t, _)| *t == 297).unwrap().1 = "5".into();
                    fields.push((300, "8".into()));
                    fields.push((58, e));
                }
                let _ = tx.send(OutMsg::Message {
                    msg_type: "b",
                    fields,
                });
            }
        }
        _ => {}
    }
}

// ---------- initiator (client side) ----------

/// client-side view of an order, tracked by the initiator and handed to
/// callbacks
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
    logout_requested: Signal,
    logged_out: Signal,
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
    control: TcpStream,
    reader_thread: Option<std::thread::JoinHandle<()>>,
    writer_thread: Option<std::thread::JoinHandle<()>>,
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
    /// Connect using the supported fresh-session / non-persistent profile.
    pub fn connect(
        cfg: InitiatorConfig,
        callback: Box<dyn Callback + Send>,
    ) -> Result<Initiator, ConnectError> {
        validate_initiator_config(&cfg).map_err(|e| {
            ConnectError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
        })?;
        let log = if cfg.log.enabled {
            SessionLog::new(
                &cfg.log.dir,
                "FIX.4.2",
                &cfg.sender_comp_id,
                &cfg.target_comp_id,
            )
            .map_err(ConnectError::Io)?
        } else {
            SessionLog::disabled()
        };
        let stream = TcpStream::connect((cfg.host.as_str(), cfg.port)).map_err(ConnectError::Io)?;
        stream.set_nodelay(true).map_err(ConnectError::Io)?;
        stream
            .set_read_timeout(Some(SOCKET_POLL))
            .map_err(ConnectError::Io)?;
        stream
            .set_write_timeout(Some(LOGON_READ_TIMEOUT))
            .map_err(ConnectError::Io)?;
        let read_stream = stream.try_clone().map_err(ConnectError::Io)?;
        read_stream
            .set_read_timeout(Some(SOCKET_POLL))
            .map_err(ConnectError::Io)?;
        let reader = frame::FrameReader::new(BufReader::new(read_stream));
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(InitiatorShared {
            logged_in: Signal::new(),
            connected: Signal::new(),
            logout_requested: Signal::new(),
            logged_out: Signal::new(),
            callback: Mutex::new(callback),
            orders: Mutex::new(HashMap::new()),
            instruments: Mutex::new(crate::core::instrument::InstrumentMap::new()),
            log: log.clone(),
        });
        shared.connected.set();
        let writer_state = WriterState {
            stream: stream.try_clone().map_err(ConnectError::Io)?,
            begin_string: "FIX.4.2".into(),
            sender_comp_id: cfg.sender_comp_id.clone(),
            target_comp_id: cfg.target_comp_id.clone(),
            seq: 0,
            log,
            wire_ids: None,
        };
        let heart = cfg.heart_bt_int;
        let failed = tx.failure_flag();
        let writer_thread = std::thread::spawn(move || {
            writer_loop(writer_state, rx, Duration::from_secs(heart as u64), failed)
        });
        let reader_shared = shared.clone();
        let reader_tx = tx.clone();
        let reader_thread =
            std::thread::spawn(move || initiator_reader(reader, reader_shared, reader_tx, cfg));
        let _ = tx.send(OutMsg::Message {
            msg_type: "A",
            fields: codec::build_logon(heart),
        });
        let mut initiator = Initiator {
            shared,
            cmd_tx: tx,
            next_order: Mutex::new(0),
            control: stream,
            reader_thread: Some(reader_thread),
            writer_thread: Some(writer_thread),
        };
        if !initiator.shared.logged_in.wait_for(LOGON_READ_TIMEOUT) {
            initiator.stop_threads();
            return Err(ConnectError::Timeout);
        }
        Ok(initiator)
    }

    fn stop_threads(&mut self) {
        let _ = self.cmd_tx.send(OutMsg::Shutdown);
        let _ = self.control.shutdown(std::net::Shutdown::Both);
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.writer_thread.take() {
            let _ = handle.join();
        }
        self.shared.connected.reset();
        self.shared.logged_in.reset();
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

    /// Wait for the peer's Logout; close only after its response or deadline.
    pub fn disconnect(&mut self) {
        if self.shared.connected.get() && !self.shared.logout_requested.get() {
            self.shared.logout_requested.set();
            let _ = self.cmd_tx.send(OutMsg::Message {
                msg_type: "5",
                fields: vec![],
            });
            self.shared.logged_out.wait_for(LOGON_READ_TIMEOUT);
        }
        self.stop_threads();
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
            .send(OutMsg::Message {
                msg_type: msg_type::NEW_ORDER_SINGLE,
                fields,
            })
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
        let fields = codec::build_cancel_replace(
            &id.to_string(),
            &req_id.to_string(),
            &symbol,
            side,
            price,
            quantity,
        );
        self.cmd_tx
            .send(OutMsg::Message {
                msg_type: msg_type::ORDER_CANCEL_REPLACE_REQUEST,
                fields,
            })
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
        let fields = codec::build_cancel_request(
            &id.to_string(),
            &req_id.to_string(),
            &record.symbol,
            record.side,
        );
        self.cmd_tx
            .send(OutMsg::Message {
                msg_type: msg_type::ORDER_CANCEL_REQUEST,
                fields,
            })
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
        let fields =
            codec::build_mass_quote(symbol, bid_price, bid_quantity, ask_price, ask_quantity);
        self.cmd_tx
            .send(OutMsg::Message {
                msg_type: msg_type::MASS_QUOTE,
                fields,
            })
            .map_err(|_| ())
    }
}

impl Drop for Initiator {
    fn drop(&mut self) {
        self.stop_threads();
    }
}

fn initiator_reader(
    mut reader: frame::FrameReader<BufReader<TcpStream>>,
    shared: Arc<InitiatorShared>,
    tx: Sender<OutMsg>,
    cfg: InitiatorConfig,
) {
    let mut sequence = SequenceState::new(1);
    let started = std::time::Instant::now();
    let mut first = true;
    let mut last_received = started;
    let (probe_after, close_after) = peer_timeouts(cfg.heart_bt_int);
    let mut pending: Option<(String, std::time::Instant)> = None;
    loop {
        if tx.failed() {
            break;
        }
        if !shared.logged_in.get() && started.elapsed() >= LOGON_READ_TIMEOUT {
            break;
        }
        if pending
            .as_ref()
            .is_some_and(|(_, deadline)| std::time::Instant::now() >= *deadline)
        {
            send_session_logout(&tx, "TestRequest timeout");
            break;
        }
        let queued = sequence.take_ready();
        let from_queue = queued.is_some();
        let received = match queued {
            Some(m) => Ok(Some(m)),
            None => reader.read_message(),
        };
        let msg = match received {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                if shared.logged_in.get()
                    && !shared.logout_requested.get()
                    && pending.is_none()
                    && last_received.elapsed() >= probe_after
                {
                    let id = format!("client-probe-{}", sequence.expected);
                    let _ = tx.send(OutMsg::Message {
                        msg_type: "1",
                        fields: codec::build_test_request(&id),
                    });
                    pending = Some((id, std::time::Instant::now() + close_after));
                }
                continue;
            }
            Err(e) => {
                shared.log.incoming_bytes(reader.buffered_bytes());
                shared.log.event(&e.to_string());
                break;
            }
        };
        if !from_queue {
            shared.log.incoming(&msg.raw);
            last_received = std::time::Instant::now();
        }
        if let Err(e) = validate_header(&msg, "FIX.4.2", &cfg.target_comp_id, &cfg.sender_comp_id) {
            send_session_logout(&tx, &e);
            break;
        }
        if first {
            if !matches!(msg.msg_type(), Some("A" | "5")) {
                send_session_logout(&tx, "expected Logon response");
                break;
            }
            first = false;
        }
        let msg = match sequence.classify(msg) {
            Ok(SequenceAction::Ready(m)) => m,
            Ok(SequenceAction::Duplicate) => continue,
            Ok(SequenceAction::Gap { request_from }) => {
                if let Some(n) = request_from {
                    let _ = tx.send(OutMsg::Message {
                        msg_type: "2",
                        fields: vec![(7, n.to_string()), (16, "0".into())],
                    });
                }
                continue;
            }
            Err(e) => {
                send_session_logout(&tx, &e);
                break;
            }
        };
        let decoded = match codec::decode(&msg) {
            Ok(v) => v,
            Err(e) => {
                sequence.expected += 1;
                send_session_reject(&tx, msg.seq(), msg.msg_type(), 5, &e);
                continue;
            }
        };
        if let Err(e) = sequence.advance(&msg) {
            send_session_logout(&tx, &e);
            break;
        }
        if msg.msg_type() == Some("0")
            && pending
                .as_ref()
                .is_some_and(|(id, _)| msg.get(112) == Some(id))
        {
            pending = None;
        }
        match decoded {
            Inbound::Logon { heart_bt_int } => {
                if shared.logged_in.get() || heart_bt_int != cfg.heart_bt_int {
                    send_session_logout(&tx, "unexpected Logon or HeartBtInt");
                    break;
                }
                shared.logged_in.set();
            }
            Inbound::Logout => {
                if !shared.logout_requested.get() {
                    let _ = tx.send(OutMsg::Message {
                        msg_type: "5",
                        fields: vec![],
                    });
                }
                shared.logged_out.set();
                break;
            }
            Inbound::Heartbeat | Inbound::SequenceReset { .. } => {}
            Inbound::TestRequest { test_req_id } => {
                let _ = tx.send(OutMsg::Message {
                    msg_type: "0",
                    fields: codec::build_heartbeat(Some(&test_req_id)),
                });
            }
            Inbound::ResendRequest => {
                let (begin_seq, end_seq) = parse_resend_range(&msg).unwrap();
                let _ = tx.send(OutMsg::GapFill { begin_seq, end_seq });
            }
            Inbound::SecurityDefinition {
                symbol,
                instrument_id,
                ..
            } => {
                let instrument = Instrument::new(instrument_id, &symbol);
                shared.instruments.lock().unwrap().put(instrument.clone());
                shared.callback.lock().unwrap().on_instrument(&instrument);
            }
            Inbound::ExecutionReport(data) => handle_execution_report(&shared, data),
            Inbound::CancelReject {
                cl_ord_id,
                orig_cl_ord_id,
                state,
                reason,
            } => {
                let mut orders = shared.orders.lock().unwrap();
                if let Ok(id) = cl_ord_id.parse::<i32>() {
                    orders.remove(&id);
                }
                let view = orig_cl_ord_id
                    .parse::<i32>()
                    .ok()
                    .and_then(|id| orders.get_mut(&id))
                    .map(|o| {
                        o.state = state;
                        o.clone()
                    });
                drop(orders);
                shared
                    .log
                    .event(&format!("Cancel/replace rejected: {reason}"));
                if let Some(view) = view {
                    shared.callback.lock().unwrap().on_order_status(&view);
                }
            }
            Inbound::BusinessReject { reason } | Inbound::SessionReject { reason } => {
                shared.log.event(&reason)
            }
            Inbound::QuoteAcknowledgement { status, reason, .. } => shared
                .log
                .event(&format!("quote status {status}: {reason}")),
            _ => send_business_reject(
                &tx,
                msg.seq(),
                msg.msg_type().unwrap(),
                3,
                "unsupported incoming message",
            ),
        }
    }
    shared.connected.reset();
    shared.logged_in.reset();
    shared.logged_out.set();
    let _ = tx.send(OutMsg::Shutdown);
}

fn handle_execution_report(shared: &InitiatorShared, data: crate::fix::codec::ExecReportData) {
    let is_quote = data.cl_ord_id.is_empty() || data.exchange_id.starts_with("quote.");
    let id = if is_quote {
        0
    } else {
        match data.cl_ord_id.parse::<i32>() {
            Ok(n) => n,
            Err(_) => {
                shared.log.event("unrecognized ClOrdID in execution report");
                return;
            }
        }
    };
    let mut view = None;
    if !is_quote {
        let mut orders = shared.orders.lock().unwrap();
        let original = data
            .orig_cl_ord_id
            .as_ref()
            .and_then(|s| s.parse::<i32>().ok());
        let template = original.and_then(|n| orders.get(&n).cloned());
        if !orders.contains_key(&id) {
            if let Some(mut old) = template {
                old.id = id;
                orders.insert(id, old);
            }
        }
        if let Some(record) = orders.get_mut(&id) {
            record.exchange_id = data.exchange_id.clone();
            record.remaining = data.remaining;
            record.price = data.price;
            record.quantity = data.quantity;
            record.state = data.state;
            view = Some(record.clone());
        }
        if let Some(original) = original.filter(|n| *n != id) {
            orders.remove(&original);
        }
    }
    if data.is_fill {
        shared.callback.lock().unwrap().on_fill(&FillView {
            is_quote,
            order_id: (!is_quote).then_some(id),
            exchange_id: data.exchange_id,
            symbol: data.symbol,
            side: data.side,
            price: data.last_price,
            quantity: data.last_quantity,
        });
    }
    if let Some(view) = view {
        shared.callback.lock().unwrap().on_order_status(&view);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sequence_message(seq: u64, kind: &str, extra: &[(u32, &str)]) -> frame::FixMessage {
        let mut fields = vec![(35, kind.into()), (34, seq.to_string())];
        fields.extend(extra.iter().map(|(tag, value)| (*tag, value.to_string())));
        frame::FixMessage {
            begin_string: "FIX.4.2".into(),
            fields,
            raw: String::new(),
        }
    }

    #[test]
    fn saturated_outbound_queue_closes_transport() {
        use std::io::Read;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let (tx, rx) =
            mpsc::channel_with_flag(1, Arc::new(std::sync::atomic::AtomicBool::new(false)));
        tx.send(OutMsg::Message {
            msg_type: "0",
            fields: vec![],
        })
        .unwrap();
        assert!(tx
            .send(OutMsg::Message {
                msg_type: "0",
                fields: vec![]
            })
            .is_err());
        let state = WriterState {
            stream,
            begin_string: "FIX.4.2".into(),
            sender_comp_id: "S".into(),
            target_comp_id: "T".into(),
            seq: 0,
            log: SessionLog::disabled(),
            wire_ids: None,
        };
        writer_loop(state, rx, Duration::from_secs(30), tx.failure_flag());
        assert_eq!(peer.read(&mut [0u8; 1]).unwrap(), 0);
    }

    #[test]
    fn direct_report_destination_overflow_does_not_block_the_other_party() {
        let mut engine = Engine::new();
        let instrument_id = engine.create_instrument("IBM");
        let (slow_tx, _slow_rx) =
            mpsc::channel_with_flag(1, Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let (fast_tx, fast_rx) = mpsc::channel();
        engine.register_session_sink("slow", FixReportSink(slow_tx.clone()));
        engine.register_session_sink("fast", FixReportSink(fast_tx.clone()));
        for (session, side) in [("slow", Side::Sell), ("fast", Side::Buy)] {
            engine
                .create_order(
                    session,
                    NewOrder {
                        id: 1,
                        instrument_id,
                        side,
                        order_type: OrderType::Limit,
                        price: Decimal::from(100),
                        quantity: Decimal::from(5),
                    },
                )
                .unwrap();
        }
        // Slow's New report filled its only slot; its Fill cannot be queued.
        // Fast still receives its Fill directly, without any report pump.
        assert!(slow_tx.failed());
        assert!(!fast_tx.failed());
        assert!(
            matches!(fast_rx.try_recv().unwrap(), OutMsg::Report(Report::Fill {
            order, last_quantity, ..
        }) if order.state == OrderState::Filled && last_quantity == Decimal::from(5))
        );
        assert!(engine.book("IBM").unwrap().bids.is_empty());
        assert!(engine.book("IBM").unwrap().asks.is_empty());
        assert_eq!(engine.statistics("IBM").unwrap().volume, Decimal::from(5));
    }

    #[test]
    fn final_logout_is_the_last_frame_even_if_more_output_was_queued() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let (tx, rx) = mpsc::channel();
        tx.send(OutMsg::LogoutAndShutdown { fields: vec![] })
            .unwrap();
        tx.send(OutMsg::Message {
            msg_type: "0",
            fields: vec![],
        })
        .unwrap();
        writer_loop(
            WriterState {
                stream,
                begin_string: "FIX.4.2".into(),
                sender_comp_id: "S".into(),
                target_comp_id: "T".into(),
                seq: 0,
                log: SessionLog::disabled(),
                wire_ids: None,
            },
            rx,
            Duration::from_secs(30),
            tx.failure_flag(),
        );
        let mut reader = frame::FrameReader::new(BufReader::new(peer));
        let logout = reader.read_message().unwrap().unwrap();
        assert_eq!(logout.msg_type(), Some("5"));
        assert_eq!(logout.seq(), Some(1));
        assert!(reader.read_message().unwrap().is_none());
    }

    #[test]
    fn gapfill_overlap_and_reset_mode_recover_without_replaying() {
        let mut state = SequenceState::new(5);
        assert!(matches!(
            state.classify(sequence_message(7, "0", &[])).unwrap(),
            SequenceAction::Gap {
                request_from: Some(5)
            }
        ));
        let overlap = sequence_message(3, "4", &[(43, "Y"), (123, "Y"), (36, "7")]);
        assert!(matches!(
            state.classify(overlap.clone()).unwrap(),
            SequenceAction::Ready(_)
        ));
        state.advance(&overlap).unwrap();
        assert_eq!(state.take_ready().unwrap().seq(), Some(7));
        assert!(matches!(
            state
                .classify(sequence_message(3, "0", &[(43, "Y")]))
                .unwrap(),
            SequenceAction::Duplicate
        ));
        assert!(state.classify(sequence_message(3, "0", &[])).is_err());
        let reset = sequence_message(99, "4", &[(36, "10")]);
        assert!(matches!(
            state.classify(reset.clone()).unwrap(),
            SequenceAction::Ready(_)
        ));
        state.advance(&reset).unwrap();
        assert_eq!(state.expected, 10);
        assert!(state
            .advance(&sequence_message(10, "4", &[(36, "9")]))
            .is_err());
    }

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
