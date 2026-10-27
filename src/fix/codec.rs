//! encode/decode of the FIX messages this exchange speaks. Field semantics
//! mirror the quickfixgo message crates.

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

// ---- decode: supported FIX 4.2 semantics with scalar field compatibility ----

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

// Scalar membership/order follows the bundled QuickFIX FIX42.xml fixture.
// This metadata identifies field boundaries and scope; it does not implement
// account, routing, settlement or other semantics for ignored optional fields.
fn scalar_fields(kind: &str) -> &'static [u32] {
    match kind {
        "A" => &[98, 108, 141, 383],
        "0" | "1" => &[112],
        "2" => &[7, 16],
        "3" => &[45, 371, 372, 373, 58],
        "4" => &[123, 36],
        "5" => &[58],
        "D" => &[
            11, 109, 76, 1, 63, 64, 21, 18, 110, 111, 100, 81, 55, 65, 48, 22, 167, 200, 205, 201,
            202, 206, 231, 223, 207, 106, 107, 140, 54, 114, 60, 38, 152, 40, 44, 99, 15, 376, 377,
            23, 117, 59, 168, 432, 126, 427, 12, 13, 47, 121, 120, 58, 193, 192, 77, 203, 204, 210,
            211, 388, 389, 439, 440,
        ],
        "F" => &[
            41, 37, 11, 66, 1, 109, 76, 55, 65, 48, 22, 167, 200, 205, 201, 202, 206, 231, 223,
            207, 106, 107, 54, 60, 38, 152, 376, 377, 58,
        ],
        "G" => &[
            37, 109, 76, 41, 11, 66, 1, 63, 64, 21, 18, 110, 111, 100, 55, 65, 48, 22, 167, 200,
            205, 201, 202, 206, 231, 223, 207, 106, 107, 54, 60, 38, 152, 40, 44, 99, 211, 388,
            389, 376, 377, 15, 59, 168, 432, 126, 427, 12, 13, 47, 121, 120, 58, 193, 192, 77, 203,
            204, 210, 114, 439, 440,
        ],
        "8" => &[
            37, 198, 11, 41, 109, 76, 66, 17, 20, 19, 150, 39, 103, 378, 1, 63, 64, 55, 65, 48, 22,
            167, 200, 205, 201, 202, 206, 231, 223, 207, 106, 107, 54, 38, 152, 40, 44, 99, 211,
            388, 389, 15, 376, 377, 59, 168, 432, 126, 18, 47, 32, 31, 194, 195, 30, 336, 29, 151,
            14, 6, 424, 425, 426, 427, 75, 60, 113, 12, 13, 381, 119, 120, 155, 156, 21, 110, 111,
            77, 210, 58, 193, 192, 439, 440, 442,
        ],
        "9" => &[37, 198, 11, 41, 39, 109, 76, 66, 1, 60, 434, 102, 58],
        "i" => &[131, 117, 301, 293, 294],
        "b" => &[131, 117, 297, 300, 301, 336, 58],
        "d" => &[
            320, 322, 323, 393, 55, 65, 48, 22, 167, 200, 205, 201, 202, 206, 231, 223, 207, 106,
            107, 15, 336, 58,
        ],
        "j" => &[45, 372, 379, 380, 58],
        _ => &[],
    }
}

const HEADER_FIELDS: &[u32] = &[
    35, 49, 56, 115, 128, 34, 50, 142, 57, 143, 116, 144, 129, 145, 43, 97, 52, 122, 347, 369, 370,
];
const QUOTE_SET_FIELDS: &[u32] = &[
    302, 311, 312, 309, 305, 310, 313, 314, 315, 316, 317, 436, 435, 308, 306, 307, 367, 304, 295,
];
const QUOTE_ENTRY_FIELDS: &[u32] = &[
    299, 55, 65, 48, 22, 167, 200, 205, 201, 202, 206, 231, 223, 207, 106, 107, 132, 133, 134, 135,
    62, 188, 190, 189, 191, 60, 336, 64, 40, 193, 192, 15,
];

fn validate_field_shape(tag: u32, kind: &str) -> Result<(), String> {
    if matches!(
        tag,
        89 | 90 | 91 | 93 | 95 | 96 | 212 | 213 | 348..=365 | 445 | 446
    ) {
        return Err(format!(
            "unsupported length/data field {tag}: cannot safely skip encoded data"
        ));
    }
    if matches!(
        tag,
        33 | 73
            | 78
            | 124
            | 136
            | 146
            | 199
            | 215
            | 267
            | 268
            | 295
            | 296
            | 382
            | 384
            | 386
            | 398
            | 420
            | 428
    ) && !(kind == "i" && matches!(tag, 295 | 296))
    {
        return Err(format!(
            "unsupported repeating group {tag}: group dictionary is not implemented"
        ));
    }
    Ok(())
}

fn validate_field_scopes(message: &super::frame::FixMessage, kind: &str) -> Result<(), String> {
    let mut body_started = false;
    // The current MassQuote contract has exactly one set and one entry. Walk
    // original fields in place so extensions cannot move a field across scopes.
    let mut quote_scope = 0;
    let mut previous = None;
    let mut needs_delimiter = false;
    for (tag, _) in &message.fields {
        validate_field_shape(*tag, kind)?;
        if HEADER_FIELDS.contains(tag) {
            if body_started {
                return Err(format!("header tag {tag} after message body"));
            }
            continue;
        }
        body_started = true;
        if kind == "i" && *tag == 296 {
            quote_scope = 1;
            previous = None;
            needs_delimiter = true;
            continue;
        }
        let scope = match quote_scope {
            1 => QUOTE_SET_FIELDS,
            2 => QUOTE_ENTRY_FIELDS,
            _ => scalar_fields(kind),
        };
        if needs_delimiter && Some(tag) != scope.first() {
            return Err(format!("missing MassQuote group delimiter {}", scope[0]));
        }
        needs_delimiter = false;
        // User-defined tags are single-value extensions by contract. No
        // custom repeating groups or length/data encoding is inferred from
        // their values; repeated custom tags are rejected by validate_profile.
        if *tag >= 5000 {
            continue;
        }
        let position = scope.iter().position(|known| known == tag).ok_or_else(|| {
            format!("unsupported tag {tag} in message {kind} scope {quote_scope}")
        })?;
        if quote_scope != 0 {
            if previous.is_some_and(|prev| position <= prev) {
                return Err("MassQuote group fields out of order".into());
            }
            previous = Some(position);
        }
        if kind == "i" && *tag == 295 {
            quote_scope = 2;
            previous = None;
            needs_delimiter = true;
        }
    }
    if needs_delimiter {
        return Err("missing MassQuote group delimiter".into());
    }
    Ok(())
}

fn validate_profile(message: &super::frame::FixMessage, kind: &str) -> Result<(), String> {
    let required: &[u32] = match kind {
        "A" => &[98, 108],
        "0" | "5" => &[],
        "1" => &[112],
        "2" => &[7, 16],
        "3" => &[45],
        "4" => &[36],
        "D" => &[11, 21, 55, 54, 60, 40, 38],
        "F" => &[11, 41, 55, 54, 60],
        "G" => &[11, 41, 21, 55, 54, 60, 40, 38],
        "8" => &[37, 17, 20, 150, 39, 55, 54, 151, 14, 6],
        "9" => &[37, 11, 41, 39, 434],
        "i" => &[117, 296, 302, 311, 304, 295, 299, 55],
        "b" => &[297],
        "d" => &[320, 322, 323, 393],
        "j" => &[372, 380],
        _ => return Ok(()),
    };
    validate_field_scopes(message, kind)?;
    let mut seen = std::collections::HashSet::new();
    for (tag, value) in &message.fields {
        if !seen.insert(*tag) {
            return Err(format!("duplicate tag {tag}"));
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
            // Scope, delimiters and dictionary order were validated in place
            // before extracting business values, including ignored extensions.
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
            "38=1|44=100|9001=a|9001=b|",
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
    fn default_compatibility_ignores_scalars_for_every_supported_message() {
        let cases = [
            "35=A|98=0|108=30|383=4096|",
            "35=0|112=probe|",
            "35=1|112=probe|",
            "35=2|7=1|16=0|",
            "35=3|45=2|373=1|",
            "35=4|36=4|123=Y|",
            "35=5|58=bye|",
            "35=D|11=a|21=1|55=AAPL|54=1|60=20260914-01:02:03|40=2|38=1|44=100|1=account|18=1|",
            "35=F|11=b|41=a|55=AAPL|54=1|60=20260914-01:02:03|1=account|",
            "35=G|11=b|41=a|21=1|55=AAPL|54=1|60=20260914-01:02:03|40=2|38=1|44=100|1=account|",
            "35=8|37=ex|17=e|20=0|150=0|39=0|55=AAPL|54=1|151=1|14=0|6=0|1=account|",
            "35=9|37=ex|11=b|41=a|39=0|434=1|1=account|",
            "35=i|117=q|131=request|9002=root|296=1|302=s|311=AAPL|312=underlying-suffix|9003=set|304=1|295=1|299=e|55=AAPL|65=suffix|9004=entry|132=99|133=101|134=10|135=10|",
            "35=b|297=0|131=request|",
            "35=d|320=r|322=s|323=4|393=1|55=AAPL|48=2|15=USD|",
            "35=j|372=D|380=0|",
        ];
        for body in cases {
            let baseline = decode(&message(body)).unwrap();
            let extended = decode(&message(&format!("{body}9001=extension|"))).unwrap();
            // The business event remains identical; retained scalar metadata
            // does not acquire any new business semantics.
            assert_eq!(format!("{baseline:?}"), format!("{extended:?}"), "{body}");
        }
        let m = message("35=D|50=sub-id|9001=before|11=a|21=1|55=AAPL|9002=middle|54=1|60=20260914-01:02:03|40=2|38=1|44=100|");
        assert!(matches!(decode(&m), Ok(Inbound::NewOrderSingle { .. })));
    }

    #[test]
    fn compatibility_keeps_semantic_and_structural_rejections() {
        let base =
            "35=D|11=a|21=1|55=AAPL|54=1|60=20260914-01:02:03|40=2|38=1|44=100|9001=extension|";
        for (from, to) in [
            ("11=a|", ""),
            ("40=2|", "40=3|"),
            ("38=1|", "38=bad|"),
            ("54=1|", "54=3|"),
            ("21=1|", "21=2|"),
        ] {
            assert!(decode(&message(&base.replace(from, to))).is_err(), "{to}");
        }
        for suffix in [
            "59=4|",
            "11=duplicate|",
            "9001=duplicate|",
            "1=a|1=b|",
            "50=late-header|",
        ] {
            assert!(
                decode(&message(&format!("{base}{suffix}"))).is_err(),
                "{suffix}"
            );
        }
        for suffix in [
            "78=1|79=account|80=1|",
            "386=1|336=session|",
            "95=4|96=data|",
            "354=4|355=data|",
        ] {
            let error = decode(&message(&format!("{base}{suffix}"))).unwrap_err();
            assert!(
                error.contains("group") || error.contains("length/data"),
                "{error}"
            );
        }
        let quote = "35=i|117=q|296=1|302=s|311=AAPL|304=1|295=1|299=e|55=AAPL|132=99|133=101|134=10|135=10|";
        for malformed in [
            quote.replace("296=1|", "296=2|"),
            quote.replace("295=1|", "295=2|"),
            quote.replace("302=s|", "9002=before-delimiter|302=s|"),
            quote.replace("299=e|", "9002=before-delimiter|299=e|"),
            quote.replace("304=1|", ""),
            quote.replace("132=99|133=101|", "133=101|9002=middle|132=99|"),
            quote.replace("311=AAPL|", "311=AAPL|55=wrong-scope|"),
            quote.replace("135=10|", "135=10|312=wrong-scope|"),
        ] {
            assert!(decode(&message(&malformed)).is_err(), "{malformed}");
        }
        for kind in ["c", "H", "V"] {
            assert!(matches!(
                decode(&message(&format!("35={kind}|9001=extension|"))),
                Ok(Inbound::Unsupported(_))
            ));
        }
    }

    #[test]
    fn compatibility_preserves_the_original_frame() {
        let body = build_new_order_single(
            OrderIdForWire("extended".into()),
            "AAPL",
            Side::Buy,
            OrderType::Limit,
            Decimal::from(100),
            Decimal::ONE,
        );
        let mut extended = body;
        extended.extend([
            (1, "external-account".into()),
            (9001, "raw-extension".into()),
        ]);
        let bytes = crate::fix::frame::frame("FIX.4.2", "D", 1, "CLIENT", "GOX", &extended);
        let parsed = crate::fix::frame::read_message(&mut std::io::Cursor::new(&bytes))
            .unwrap()
            .unwrap();
        assert!(matches!(
            decode(&parsed),
            Ok(Inbound::NewOrderSingle { .. })
        ));
        assert_eq!(parsed.raw.as_bytes(), bytes);
        assert_eq!(parsed.get(9001), Some("raw-extension"));
        assert_eq!(parsed.get(1), Some("external-account"));
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
