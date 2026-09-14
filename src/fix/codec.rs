//! encode/decode of the FIX messages this exchange speaks. Field semantics
//! mirror the Go implementation (quickfixgo message crates + common/fix.go).

use rust_decimal::Decimal;

use crate::core::exchange::{ExecType, Report};
use crate::core::order::{OrderState, OrderType, Side};

pub mod msg_type {
    pub const HEARTBEAT: &str = "0";
    pub const TEST_REQUEST: &str = "1";
    pub const RESEND_REQUEST: &str = "2";
    pub const SEQUENCE_RESET: &str = "4";
    pub const LOGOUT: &str = "5";
    pub const REJECT: &str = "3";
    pub const ORDER_CANCEL_REJECT: &str = "9";
    pub const EXECUTION_REPORT: &str = "8";
    pub const LOGON: &str = "A";
    pub const NEW_ORDER_SINGLE: &str = "D";
    pub const ORDER_CANCEL_REQUEST: &str = "F";
    pub const ORDER_CANCEL_REPLACE_REQUEST: &str = "G";
    /// MassQuote is "i" in both FIX 4.2 and 4.4 (verified against the official
    /// FIX42.xml dictionary: `<message name='MassQuote' msgtype='i'>`)
    pub const MASS_QUOTE: &str = "i";
    pub const MASS_QUOTE_ACKNOWLEDGEMENT: &str = "b";
    pub const SECURITY_DEFINITION_REQUEST: &str = "c";
    /// FIX 4.2: SecurityDefinition is "d" ("y" is SecurityList)
    pub const SECURITY_DEFINITION: &str = "d";
    /// BusinessMessageReject supplements session Reject (3) and order-specific rejects.
    pub const BUSINESS_MESSAGE_REJECT: &str = "j";
    // NOTE: SecurityListRequest ("x") does not exist in FIX 4.2 - instruments
}

pub mod tags {
    pub const CL_ORD_ID: u32 = 11;
    pub const CUM_QTY: u32 = 14;
    pub const EXEC_ID: u32 = 17;
    pub const LAST_PX: u32 = 31;
    pub const LAST_QTY: u32 = 32;
    pub const ORDER_ID: u32 = 37;
    pub const ORDER_QTY: u32 = 38;
    pub const ORD_STATUS: u32 = 39;
    pub const ORD_TYPE: u32 = 40;
    pub const ORIG_CL_ORD_ID: u32 = 41;
    pub const PRICE: u32 = 44;
    pub const SECURITY_ID: u32 = 48;
    pub const SIDE: u32 = 54;
    pub const SYMBOL: u32 = 55;
    pub const TEXT: u32 = 58;
    pub const TRANSACT_TIME: u32 = 60;
    pub const HANDL_INST: u32 = 21;
    pub const EXEC_TRANS_TYPE: u32 = 20;
    pub const TOTAL_NUM_SECURITIES: u32 = 393;
    pub const UNDERLYING_SYMBOL: u32 = 311;
    pub const TOT_QUOTE_ENTRIES: u32 = 304;
    pub const NEW_SEQ_NO: u32 = 36;
    pub const ENCRYPT_METHOD: u32 = 98;
    pub const HEART_BT_INT: u32 = 108;
    pub const TEST_REQ_ID: u32 = 112;
    pub const QUOTE_ID: u32 = 117;
    pub const GAP_FILL_FLAG: u32 = 123;
    pub const BID_PX: u32 = 132;
    pub const OFFER_PX: u32 = 133;
    pub const BID_SIZE: u32 = 134;
    pub const OFFER_SIZE: u32 = 135;
    pub const EXEC_TYPE: u32 = 150;
    pub const LEAVES_QTY: u32 = 151;
    /// MassQuote group tags (identical in FIX 4.2 and 4.4)
    pub const QUOTE_ENTRY_ID: u32 = 299;
    pub const NO_QUOTE_ENTRIES: u32 = 295;
    pub const QUOTE_SET_ID: u32 = 302;
    pub const NO_QUOTE_SETS: u32 = 296;
    pub const QUOTE_STATUS: u32 = 297;
    pub const QUOTE_RESPONSE_LEVEL: u32 = 301;
    pub const SECURITY_REQ_ID: u32 = 320;
    pub const SECURITY_RESPONSE_ID: u32 = 322;
    /// SecurityResponseType, used in SecurityDefinition (d)
    pub const SECURITY_RESPONSE_TYPE: u32 = 323;
}

/// Preserve supported Decimal precision; never round orders on the wire.
pub fn fix_decimal(d: Decimal) -> String {
    d.normalize().to_string()
}

pub fn parse_decimal(s: &str) -> Result<Decimal, String> {
    let digits = s.strip_prefix('-').unwrap_or(s);
    if digits.is_empty()
        || !digits.bytes().any(|c| c.is_ascii_digit())
        || digits.bytes().filter(|c| *c == b'.').count() > 1
        || !digits.bytes().all(|c| c.is_ascii_digit() || c == b'.')
    {
        return Err(format!("invalid FIX decimal {s:?}"));
    }
    s.parse::<Decimal>()
        .map_err(|e| format!("bad decimal {s:?}: {e}"))
}

pub fn side_to_fix(side: Side) -> &'static str {
    match side {
        Side::Buy => "1",
        Side::Sell => "2",
    }
}

pub fn side_from_fix(s: &str) -> Result<Side, String> {
    match s {
        "1" => Ok(Side::Buy),
        "2" => Ok(Side::Sell),
        _ => Err(format!("unsupported side {}", s)),
    }
}

pub fn ord_status_to_fix(state: OrderState) -> &'static str {
    match state {
        OrderState::Booked | OrderState::New => "0",
        OrderState::PartialFill => "1",
        OrderState::Filled => "2",
        OrderState::Cancelled => "4",
        OrderState::Rejected => "8",
        OrderState::Expired => "C",
    }
}

pub fn ord_status_from_fix(s: &str) -> Result<OrderState, String> {
    match s {
        "0" | "5" => Ok(OrderState::Booked),
        "1" => Ok(OrderState::PartialFill),
        "2" => Ok(OrderState::Filled),
        "4" => Ok(OrderState::Cancelled),
        "8" => Ok(OrderState::Rejected),
        "C" => Ok(OrderState::Expired),
        _ => Err(format!("unsupported order status {}", s)),
    }
}

fn transact_time() -> String {
    chrono::Utc::now().format("%Y%m%d-%H:%M:%S%.3f").to_string()
}

fn message_identifier(prefix: &str) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    format!(
        "{prefix}-{}-{}",
        chrono::Utc::now().timestamp_micros(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

// ---- initiator -> exchange ----

pub fn build_logon(heart_bt_int: u32) -> Vec<(u32, String)> {
    vec![
        (tags::ENCRYPT_METHOD, "0".to_string()),
        (tags::HEART_BT_INT, heart_bt_int.to_string()),
    ]
}

pub fn build_logout() -> Vec<(u32, String)> {
    vec![]
}

pub fn build_heartbeat(test_req_id: Option<&str>) -> Vec<(u32, String)> {
    match test_req_id {
        Some(id) => vec![(tags::TEST_REQ_ID, id.to_string())],
        None => vec![],
    }
}

pub fn build_test_request(req_id: &str) -> Vec<(u32, String)> {
    vec![(tags::TEST_REQ_ID, req_id.to_string())]
}

pub fn build_new_order_single(
    cl_ord_id: OrderIdForWire,
    symbol: &str,
    side: Side,
    order_type: OrderType,
    price: Decimal,
    quantity: Decimal,
) -> Vec<(u32, String)> {
    let mut fields = vec![
        (tags::CL_ORD_ID, cl_ord_id.0),
        (tags::HANDL_INST, "1".to_string()), // required in FIX 4.2
        (tags::SYMBOL, symbol.to_string()),
        (tags::SIDE, side_to_fix(side).to_string()),
        (tags::TRANSACT_TIME, transact_time()),
        (
            tags::ORD_TYPE,
            match order_type {
                OrderType::Limit => "2".to_string(),
                OrderType::Market => "1".to_string(),
            },
        ),
        (tags::ORDER_QTY, fix_decimal(quantity)),
    ];
    if order_type == OrderType::Limit {
        fields.push((tags::PRICE, fix_decimal(price)));
    }
    fields
}

pub struct OrderIdForWire(pub String);

/// OrderCancelRequest: ClOrdID is the request's own fresh id; OrigClOrdID
/// identifies the order being cancelled
pub fn build_cancel_request(
    orig_cl_ord_id: &str,
    cl_ord_id: &str,
    symbol: &str,
    side: Side,
) -> Vec<(u32, String)> {
    vec![
        (tags::ORIG_CL_ORD_ID, orig_cl_ord_id.to_string()),
        (tags::CL_ORD_ID, cl_ord_id.to_string()),
        (tags::SYMBOL, symbol.to_string()),
        (tags::SIDE, side_to_fix(side).to_string()),
        (tags::TRANSACT_TIME, transact_time()),
    ]
}

/// OrderCancelReplaceRequest: ClOrdID is the fresh request id and becomes the
/// replacement order's ClOrdID; OrigClOrdID identifies the original order
pub fn build_cancel_replace(
    orig_cl_ord_id: &str,
    cl_ord_id: &str,
    symbol: &str,
    side: Side,
    price: Decimal,
    quantity: Decimal,
) -> Vec<(u32, String)> {
    vec![
        (tags::ORIG_CL_ORD_ID, orig_cl_ord_id.to_string()),
        (tags::CL_ORD_ID, cl_ord_id.to_string()),
        (tags::HANDL_INST, "1".to_string()), // required in FIX 4.2
        (tags::SYMBOL, symbol.to_string()),
        (tags::SIDE, side_to_fix(side).to_string()),
        (tags::TRANSACT_TIME, transact_time()),
        (tags::ORD_TYPE, "2".to_string()),
        (tags::ORDER_QTY, fix_decimal(quantity)),
        (tags::PRICE, fix_decimal(price)),
    ]
}

pub fn build_mass_quote(
    symbol: &str,
    bid_price: Decimal,
    bid_quantity: Decimal,
    ask_price: Decimal,
    ask_quantity: Decimal,
) -> Vec<(u32, String)> {
    vec![
        (tags::QUOTE_ID, message_identifier("q")),
        (tags::NO_QUOTE_SETS, "1".to_string()),
        (tags::QUOTE_SET_ID, "1".to_string()),
        (tags::UNDERLYING_SYMBOL, symbol.to_string()), // required in FIX 4.2
        (tags::TOT_QUOTE_ENTRIES, "1".to_string()),    // required in FIX 4.2
        (tags::NO_QUOTE_ENTRIES, "1".to_string()),
        (tags::QUOTE_ENTRY_ID, symbol.to_string()),
        (tags::SYMBOL, symbol.to_string()),
        (tags::BID_PX, fix_decimal(bid_price)),
        (tags::OFFER_PX, fix_decimal(ask_price)),
        (tags::BID_SIZE, fix_decimal(bid_quantity)),
        (tags::OFFER_SIZE, fix_decimal(ask_quantity)),
    ]
}

// ---- exchange -> initiator ----

/// Encode the execution event separately from the current order state.
pub fn build_execution_report(report: &Report) -> Vec<(u32, String)> {
    let (order, symbol, cl_ord_id, exec_id, event, original, fill) = match report {
        Report::Status {
            order,
            symbol,
            cl_ord_id,
            exec_id,
            exec_type,
            orig_cl_ord_id,
        } => (
            order,
            symbol,
            *cl_ord_id,
            exec_id,
            *exec_type,
            *orig_cl_ord_id,
            None,
        ),
        Report::Fill {
            order,
            symbol,
            last_price,
            last_quantity,
            exec_id,
            orig_cl_ord_id,
        } => (
            order,
            symbol,
            order.id,
            exec_id,
            if order.remaining.is_zero() {
                ExecType::Fill
            } else {
                ExecType::PartialFill
            },
            *orig_cl_ord_id,
            Some((*last_price, *last_quantity)),
        ),
    };
    let event = match event {
        ExecType::New => "0",
        ExecType::PartialFill => "1",
        ExecType::Fill => "2",
        ExecType::Cancelled => "4",
        ExecType::Replaced => "5",
        ExecType::Rejected => "8",
        ExecType::Expired => "C",
    };
    let status = if event == "5" && order.cum_quantity.is_zero() {
        "5"
    } else {
        ord_status_to_fix(order.state)
    };
    let mut fields = vec![
        (37, order.exchange_id.clone()),
        (17, exec_id.clone()),
        (20, "0".into()),
        (150, event.into()),
        (39, status.into()),
        (55, symbol.clone()),
        (54, side_to_fix(order.side).into()),
        (38, fix_decimal(order.quantity)),
        (
            40,
            if order.order_type == OrderType::Market {
                "1"
            } else {
                "2"
            }
            .into(),
        ),
        (
            151,
            fix_decimal(if order.state.is_active() {
                order.remaining
            } else {
                Decimal::ZERO
            }),
        ),
        (14, fix_decimal(order.cum_quantity)),
        (6, fix_decimal(order.avg_price)),
        (60, transact_time()),
    ];
    if cl_ord_id != 0 {
        fields.push((11, cl_ord_id.to_string()));
    }
    if let Some(original) = original {
        fields.push((41, original.to_string()));
    }
    if order.order_type == OrderType::Limit {
        fields.push((44, fix_decimal(order.price)));
    }
    let (px, qty) = fill.unwrap_or((Decimal::ZERO, Decimal::ZERO));
    fields.push((31, fix_decimal(px)));
    fields.push((32, fix_decimal(qty)));
    fields
}

pub fn build_security_definition(
    req_id: &str,
    symbol: &str,
    instrument_id: i64,
) -> Vec<(u32, String)> {
    vec![
        (tags::SECURITY_REQ_ID, req_id.to_string()),
        (tags::SECURITY_RESPONSE_ID, message_identifier("security")),
        (tags::TOTAL_NUM_SECURITIES, "1".to_string()), // required in FIX 4.2
        (tags::SECURITY_RESPONSE_TYPE, "4".to_string()),
        (tags::SYMBOL, symbol.to_string()),
        (tags::SECURITY_ID, instrument_id.to_string()),
        (22, "8".to_string()),
    ]
}

pub fn build_mass_quote_ack(quote_id: &str) -> Vec<(u32, String)> {
    vec![
        (tags::QUOTE_STATUS, "0".to_string()),
        (tags::QUOTE_ID, quote_id.to_string()),
    ]
}

// ---- decode: strict supported FIX 4.2 profile ----

#[derive(Debug)]
pub enum Inbound {
    Logon {
        heart_bt_int: u32,
    },
    Logout,
    Heartbeat,
    TestRequest {
        test_req_id: String,
    },
    ResendRequest,
    SequenceReset {
        new_seq_no: u64,
        gap_fill: bool,
    },
    SessionReject {
        reason: String,
    },
    NewOrderSingle {
        cl_ord_id: String,
        symbol: String,
        side: Side,
        order_type: OrderType,
        price: Decimal,
        quantity: Decimal,
    },
    CancelRequest {
        cl_ord_id: String,
        orig_cl_ord_id: String,
        symbol: String,
        side: Side,
    },
    CancelReplace {
        cl_ord_id: String,
        orig_cl_ord_id: String,
        symbol: String,
        side: Side,
        order_type: OrderType,
        price: Decimal,
        quantity: Decimal,
    },
    CancelReject {
        cl_ord_id: String,
        orig_cl_ord_id: String,
        state: OrderState,
        reason: String,
    },
    MassQuote {
        quote_id: String,
        response_level: u8,
        symbol: String,
        bid_px: Decimal,
        bid_qty: Decimal,
        offer_px: Decimal,
        offer_qty: Decimal,
    },
    QuoteAcknowledgement {
        quote_id: Option<String>,
        status: String,
        reason: String,
    },
    SecurityDefinition {
        req_id: String,
        symbol: String,
        instrument_id: i64,
    },
    BusinessReject {
        reason: String,
    },
    ExecutionReport(ExecReportData),
    Unsupported(String),
}

#[derive(Debug)]
pub struct ExecReportData {
    pub exchange_id: String,
    pub cl_ord_id: String,
    pub orig_cl_ord_id: Option<String>,
    pub symbol: String,
    pub state: OrderState,
    pub is_fill: bool,
    pub side: Side,
    pub price: Decimal,
    pub quantity: Decimal,
    pub remaining: Decimal,
    pub last_price: Decimal,
    pub last_quantity: Decimal,
}

fn validate_timestamp(value: &str) -> Result<(), String> {
    if value.len() != 17 && value.len() != 21 {
        return Err("FIX 4.2 timestamp requires seconds or milliseconds".into());
    }
    chrono::NaiveDateTime::parse_from_str(
        value,
        if value.len() == 17 {
            "%Y%m%d-%H:%M:%S"
        } else {
            "%Y%m%d-%H:%M:%S%.3f"
        },
    )
    .map(|_| ())
    .map_err(|_| "invalid UTCTimestamp".into())
}

fn validate_profile(message: &super::frame::FixMessage, kind: &str) -> Result<(), String> {
    let (required, allowed): (&[u32], &[u32]) = match kind {
        "A" => (&[98, 108], &[98, 108, 141]),
        "0" => (&[], &[112]),
        "1" => (&[112], &[112]),
        "2" => (&[7, 16], &[7, 16]),
        "3" => (&[45], &[45, 371, 372, 373, 58]),
        "4" => (&[36], &[36, 123]),
        "5" => (&[], &[58]),
        "D" => (
            &[11, 21, 55, 54, 60, 40, 38],
            &[11, 21, 55, 54, 60, 40, 38, 44, 59],
        ),
        "F" => (&[11, 41, 55, 54, 60], &[11, 41, 55, 54, 60, 38]),
        "G" => (
            &[11, 41, 21, 55, 54, 60, 40, 38],
            &[11, 41, 21, 55, 54, 60, 40, 38, 44, 59],
        ),
        "8" => (
            &[37, 17, 20, 150, 39, 55, 54, 151, 14, 6],
            &[
                37, 17, 20, 150, 39, 55, 54, 151, 14, 6, 11, 41, 38, 40, 44, 31, 32, 60, 103, 58,
            ],
        ),
        "9" => (&[37, 11, 41, 39, 434], &[37, 11, 41, 39, 434, 102, 58, 60]),
        "i" => (
            &[117, 296, 302, 311, 304, 295, 299, 55],
            &[
                117, 301, 296, 302, 311, 304, 295, 299, 55, 132, 133, 134, 135,
            ],
        ),
        "b" => (&[297], &[117, 297, 300, 58]),
        "d" => (&[320, 322, 323, 393], &[320, 322, 323, 393, 55, 48, 22]),
        "j" => (&[372, 380], &[45, 372, 379, 380, 58]),
        _ => return Ok(()),
    };
    let header = [35, 34, 49, 56, 52, 43, 122];
    let mut seen = std::collections::HashSet::new();
    for (tag, value) in &message.fields {
        if !seen.insert(*tag) {
            return Err(format!("duplicate tag {tag}"));
        }
        if !header.contains(tag) && !allowed.contains(tag) {
            return Err(format!("unsupported tag {tag} for message {kind}"));
        }
        if value.is_empty() {
            return Err(format!("empty tag {tag}"));
        }
        let valid = match tag {
            373 => matches!(
                value.as_str(),
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "10" | "11"
            ),
            380 => matches!(value.as_str(), "0" | "1" | "2" | "3" | "4" | "5"),
            102 => matches!(value.as_str(), "0" | "1" | "2" | "3"),
            300 => matches!(
                value.as_str(),
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"
            ),
            _ => true,
        };
        if !valid {
            return Err(format!("invalid FIX 4.2 enumeration tag {tag}"));
        }
    }
    for tag in required {
        if !seen.contains(tag) {
            return Err(format!("missing required tag {tag} in {kind}"));
        }
    }
    if let Some(t) = message.get(60) {
        validate_timestamp(t)?;
    }
    if matches!(kind, "D" | "G") {
        if message.get(21) != Some("1") {
            return Err("only automated private execution HandlInst=1 is supported".into());
        }
        if message.get(59).unwrap_or("0") != "0" {
            return Err("only DAY TimeInForce=0 is supported".into());
        }
    }
    Ok(())
}

pub fn decode(message: &super::frame::FixMessage) -> Result<Inbound, String> {
    let kind = message.msg_type().ok_or("missing MsgType(35)")?;
    validate_profile(message, kind)?;
    let get = |tag| message.get(tag);
    let need = |tag| get(tag).ok_or_else(|| format!("missing tag {tag}"));
    let text = |tag| need(tag).map(str::to_string);
    let dec = |tag| parse_decimal(need(tag)?);
    let positive = |tag| -> Result<Decimal, String> {
        let value = dec(tag)?;
        if value <= Decimal::ZERO {
            return Err(format!("tag {tag} must be positive"));
        }
        Ok(value)
    };
    let natural = |tag| -> Result<u64, String> {
        let value = need(tag)?;
        if !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("invalid integer tag {tag}"));
        }
        value
            .parse::<u64>()
            .map_err(|_| format!("invalid integer tag {tag}"))
    };
    let boolean = |tag, default| -> Result<bool, String> {
        match get(tag) {
            None => Ok(default),
            Some("Y") => Ok(true),
            Some("N") => Ok(false),
            _ => Err(format!("invalid boolean tag {tag}")),
        }
    };
    let order_type = || -> Result<OrderType, String> {
        match need(40)? {
            "1" => Ok(OrderType::Market),
            "2" => Ok(OrderType::Limit),
            _ => Err("unsupported OrdType(40)".into()),
        }
    };
    let order_price = |typ| -> Result<Decimal, String> {
        if typ == OrderType::Limit {
            positive(44)
        } else {
            get(44)
                .map(parse_decimal)
                .transpose()
                .map(|x| x.unwrap_or(Decimal::ZERO))
        }
    };
    match kind {
        "A" => {
            if need(98)? != "0" {
                return Err("only EncryptMethod=0 is supported".into());
            }
            boolean(141, false)?;
            let heart = u32::try_from(natural(108)?).map_err(|_| "invalid HeartBtInt")?;
            if heart == 0 {
                return Err("HeartBtInt must be positive in this profile".into());
            }
            Ok(Inbound::Logon {
                heart_bt_int: heart,
            })
        }
        "0" => Ok(Inbound::Heartbeat),
        "1" => Ok(Inbound::TestRequest {
            test_req_id: text(112)?,
        }),
        "2" => {
            let start = natural(7)?;
            let end = natural(16)?;
            if start == 0 || (end != 0 && end < start) {
                return Err("invalid ResendRequest range".into());
            }
            Ok(Inbound::ResendRequest)
        }
        "3" => {
            natural(45)?;
            Ok(Inbound::SessionReject {
                reason: get(58).unwrap_or("").into(),
            })
        }
        "4" => {
            let n = natural(36)?;
            if n == 0 {
                return Err("NewSeqNo must be positive".into());
            }
            Ok(Inbound::SequenceReset {
                new_seq_no: n,
                gap_fill: boolean(123, false)?,
            })
        }
        "5" => Ok(Inbound::Logout),
        "D" => {
            let typ = order_type()?;
            Ok(Inbound::NewOrderSingle {
                cl_ord_id: text(11)?,
                symbol: text(55)?,
                side: side_from_fix(need(54)?)?,
                order_type: typ,
                price: order_price(typ)?,
                quantity: positive(38)?,
            })
        }
        "F" => {
            if get(38).is_some() {
                positive(38)?;
            }
            Ok(Inbound::CancelRequest {
                cl_ord_id: text(11)?,
                orig_cl_ord_id: text(41)?,
                symbol: text(55)?,
                side: side_from_fix(need(54)?)?,
            })
        }
        "G" => {
            let typ = order_type()?;
            Ok(Inbound::CancelReplace {
                cl_ord_id: text(11)?,
                orig_cl_ord_id: text(41)?,
                symbol: text(55)?,
                side: side_from_fix(need(54)?)?,
                order_type: typ,
                price: order_price(typ)?,
                quantity: positive(38)?,
            })
        }
        "9" => {
            if !matches!(need(434)?, "1" | "2") {
                return Err("invalid CxlRejResponseTo".into());
            }
            Ok(Inbound::CancelReject {
                cl_ord_id: text(11)?,
                orig_cl_ord_id: text(41)?,
                state: ord_status_from_fix(need(39)?)?,
                reason: get(58).unwrap_or("").into(),
            })
        }
        "i" => {
            if need(296)? != "1" || need(295)? != "1" || need(304)? != "1" {
                return Err("only a single complete quote set and entry is supported".into());
            }
            // FIX repeating groups must preserve the dictionary field order.
            let order = [302, 311, 304, 295, 299, 55, 132, 133, 134, 135];
            let mut previous = None;
            let start = message.fields.iter().position(|(t, _)| *t == 296).unwrap();
            for (tag, _) in &message.fields[start + 1..] {
                let pos = order
                    .iter()
                    .position(|t| t == tag)
                    .ok_or("field outside supported MassQuote group")?;
                if previous.is_some_and(|prev| pos <= prev) {
                    return Err("MassQuote group fields out of order".into());
                }
                previous = Some(pos);
            }
            let level = match get(301).unwrap_or("0") {
                "0" => 0,
                "1" => 1,
                "2" => 2,
                _ => return Err("invalid QuoteResponseLevel".into()),
            };
            // This profile maintains a two-sided quote snapshot. Missing sides
            // are withdrawals, as are explicit zero price/size values.
            let quote_value = |tag| -> Result<Decimal, String> {
                let value = get(tag)
                    .map(parse_decimal)
                    .transpose()?
                    .unwrap_or(Decimal::ZERO);
                if value < Decimal::ZERO {
                    return Err(format!("negative quote tag {tag}"));
                }
                Ok(value)
            };
            Ok(Inbound::MassQuote {
                quote_id: text(117)?,
                response_level: level,
                symbol: text(55)?,
                bid_px: quote_value(132)?,
                bid_qty: quote_value(134)?,
                offer_px: quote_value(133)?,
                offer_qty: quote_value(135)?,
            })
        }
        "d" => {
            if need(323)? != "4" {
                return Err("only SecurityDefinition list responses are supported".into());
            }
            natural(393)?;
            Ok(Inbound::SecurityDefinition {
                req_id: text(320)?,
                symbol: text(55)?,
                instrument_id: need(48)?
                    .parse()
                    .map_err(|_| "invalid SecurityID for supported instrument profile")?,
            })
        }
        "8" => {
            if need(20)? != "0" {
                return Err("only new execution transactions are supported".into());
            }
            if !matches!(need(150)?, "0" | "1" | "2" | "4" | "5" | "8" | "C") {
                return Err("unsupported ExecType".into());
            }
            if matches!(need(150)?, "4" | "5") && get(11).is_some() && get(41).is_none() {
                // Unsolicited cancellations (for example market remainder)
                // need not identify a cancel request. Replacements always do.
                if need(150)? == "5" {
                    return Err("replacement missing OrigClOrdID".into());
                }
            }
            let is_fill = matches!(need(150)?, "1" | "2");
            let cum = dec(14)?;
            let remaining = dec(151)?;
            let avg = dec(6)?;
            if cum < Decimal::ZERO || remaining < Decimal::ZERO || avg < Decimal::ZERO {
                return Err("negative execution aggregate".into());
            }
            let last_price = if is_fill {
                positive(31)?
            } else {
                get(31)
                    .map(parse_decimal)
                    .transpose()?
                    .unwrap_or(Decimal::ZERO)
            };
            let last_quantity = if is_fill {
                positive(32)?
            } else {
                get(32)
                    .map(parse_decimal)
                    .transpose()?
                    .unwrap_or(Decimal::ZERO)
            };
            Ok(Inbound::ExecutionReport(ExecReportData {
                exchange_id: text(37)?,
                cl_ord_id: get(11).unwrap_or("").into(),
                orig_cl_ord_id: get(41).map(str::to_string),
                symbol: text(55)?,
                state: ord_status_from_fix(need(39)?)?,
                is_fill,
                side: side_from_fix(need(54)?)?,
                price: get(44)
                    .map(parse_decimal)
                    .transpose()?
                    .unwrap_or(Decimal::ZERO),
                quantity: get(38)
                    .map(parse_decimal)
                    .transpose()?
                    .unwrap_or(cum + remaining),
                remaining,
                last_price,
                last_quantity,
            }))
        }
        "j" => {
            natural(380)?;
            Ok(Inbound::BusinessReject {
                reason: get(58).unwrap_or("").into(),
            })
        }
        "b" => {
            if !matches!(need(297)?, "0" | "1" | "2" | "3" | "4" | "5") {
                return Err("invalid QuoteStatus".into());
            }
            // A supported informational response, not a request to execute.
            Ok(Inbound::QuoteAcknowledgement {
                quote_id: get(117).map(str::to_string),
                status: text(297)?,
                reason: get(58).unwrap_or("").into(),
            })
        }
        other => Ok(Inbound::Unsupported(other.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fix::frame::{parse_body, FixMessage};
    fn message(body: &str) -> FixMessage {
        FixMessage {
            begin_string: "FIX.4.2".into(),
            fields: parse_body(&body.replace('|', "\x01")),
            raw: String::new(),
        }
    }

    #[test]
    fn standard_ordtype_is_not_a_self_roundtrip() {
        let base = "35=D|11=order-alpha|21=1|55=AAPL|54=1|60=20260914-01:02:03|38=10|";
        match decode(&message(&format!("{base}40=2|44=100|"))).unwrap() {
            Inbound::NewOrderSingle {
                order_type,
                cl_ord_id,
                ..
            } => {
                assert_eq!(order_type, OrderType::Limit);
                assert_eq!(cl_ord_id, "order-alpha");
            }
            _ => panic!(),
        }
        match decode(&message(&format!("{base}40=1|"))).unwrap() {
            Inbound::NewOrderSingle { order_type, .. } => assert_eq!(order_type, OrderType::Market),
            _ => panic!(),
        }
        assert!(build_new_order_single(
            OrderIdForWire("1".into()),
            "AAPL",
            Side::Buy,
            OrderType::Limit,
            Decimal::ONE,
            Decimal::ONE
        )
        .contains(&(40, "2".into())));
    }

    #[test]
    fn invalid_supported_orders_are_rejected() {
        let base = "35=D|11=a|21=1|55=AAPL|54=1|60=20260914-01:02:03|40=2|";
        for suffix in [
            "38=10|",
            "38=-1|44=100|",
            "38=1|44=wrong|",
            "38=1|44=100|59=4|",
            "38=1|44=100|11=duplicate|",
            "38=1|44=100|18=1|",
        ] {
            assert!(
                decode(&message(&format!("{base}{suffix}"))).is_err(),
                "accepted {suffix}"
            );
        }
        assert!(decode(&message("35=G|11=b|55=AAPL|54=1|40=2|38=1|44=1|")).is_err());
    }

    #[test]
    fn report_includes_average_and_supplied_execution_identity() {
        let mut order = crate::core::order::Order::limit(
            "s",
            1,
            1,
            Side::Buy,
            Decimal::from(100),
            Decimal::from(10),
            1,
        );
        order.exchange_id = "ex-1".into();
        order.cum_quantity = Decimal::from(4);
        order.remaining = Decimal::from(6);
        order.avg_price = Decimal::from(100);
        order.state = OrderState::PartialFill;
        let report = Report::Status {
            order,
            symbol: "AAPL".into(),
            cl_ord_id: 2,
            exec_id: "event-unique".into(),
            exec_type: ExecType::Replaced,
            orig_cl_ord_id: Some(1),
        };
        let fields = build_execution_report(&report);
        for pair in [
            (6, "100"),
            (17, "event-unique"),
            (150, "5"),
            (39, "1"),
            (11, "2"),
            (41, "1"),
            (14, "4"),
            (151, "6"),
        ] {
            assert!(fields.contains(&(pair.0, pair.1.into())));
        }
    }

    #[test]
    fn quote_response_level_and_order_are_standard() {
        let m = message("35=i|117=q|301=2|296=1|302=s|311=AAPL|304=1|295=1|299=e|55=AAPL|132=99|133=101|134=10|135=10|");
        assert!(matches!(
            decode(&m).unwrap(),
            Inbound::MassQuote {
                response_level: 2,
                ..
            }
        ));
        let bad =
            message("35=i|117=q|296=1|302=s|311=AAPL|304=1|295=1|299=e|55=AAPL|134=10|132=99|");
        assert!(decode(&bad).is_err());
    }

    #[test]
    fn reject_messages_require_fix42_fields() {
        assert!(decode(&message("35=j|58=oops|")).is_err());
        assert!(matches!(
            decode(&message("35=j|372=D|380=0|58=oops|")).unwrap(),
            Inbound::BusinessReject { .. }
        ));
        assert!(matches!(
            decode(&message("35=3|45=2|373=1|58=oops|")).unwrap(),
            Inbound::SessionReject { .. }
        ));
    }

    #[test]
    fn decimal_wire_precision_is_preserved() {
        assert_eq!(fix_decimal("1.12345678".parse().unwrap()), "1.12345678");
    }
}
