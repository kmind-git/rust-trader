//! encode/decode of the FIX messages this exchange speaks. Field semantics
//! mirror the Go implementation (quickfixgo message crates + common/fix.go).

use rust_decimal::Decimal;

use crate::core::exchange::Report;
use crate::core::order::{OrderState, OrderType, Side};

pub mod msg_type {
    pub const HEARTBEAT: &str = "0";
    pub const TEST_REQUEST: &str = "1";
    pub const RESEND_REQUEST: &str = "2";
    pub const SEQUENCE_RESET: &str = "4";
    pub const LOGOUT: &str = "5";
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
    /// FIX 4.2 has no SessionReject; business-level problems use this
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

/// FIX numeric fields carry 4 decimal places, matching quickfixgo's
/// `ToDecimal(x, 4)` output (e.g. "100.0000")
pub fn fix_decimal(d: Decimal) -> String {
    let mut d = d.round_dp(4);
    d.rescale(4);
    d.to_string()
}

pub fn parse_decimal(s: &str) -> Result<Decimal, String> {
    s.parse::<Decimal>().map_err(|e| format!("bad decimal {:?}: {}", s, e))
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
    }
}

pub fn ord_status_from_fix(s: &str) -> Result<OrderState, String> {
    match s {
        "0" => Ok(OrderState::Booked),
        "1" => Ok(OrderState::PartialFill),
        "2" => Ok(OrderState::Filled),
        "4" => Ok(OrderState::Cancelled),
        "8" => Ok(OrderState::Rejected),
        _ => Err(format!("unsupported order status {}", s)),
    }
}

fn transact_time() -> String {
    chrono::Utc::now().format("%Y%m%d-%H:%M:%S%.3f").to_string()
}

// ---- initiator -> exchange ----

pub fn build_logon(heart_bt_int: u32) -> Vec<(u32, String)> {
    vec![(tags::ENCRYPT_METHOD, "0".to_string()), (tags::HEART_BT_INT, heart_bt_int.to_string())]
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
                OrderType::Limit => "1".to_string(),
                OrderType::Market => "2".to_string(),
            },
        ),
        (tags::ORDER_QTY, fix_decimal(quantity)),
    ];
    // Go always sends Price (0 for market orders)
    fields.push((tags::PRICE, fix_decimal(price)));
    fields
}

pub struct OrderIdForWire(pub String);

/// OrderCancelRequest: ClOrdID is the request's own fresh id; OrigClOrdID
/// identifies the order being cancelled
pub fn build_cancel_request(orig_cl_ord_id: &str, cl_ord_id: &str, symbol: &str, side: Side) -> Vec<(u32, String)> {
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
        (tags::ORD_TYPE, "1".to_string()),
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
        (tags::QUOTE_ID, "1".to_string()),
        (tags::NO_QUOTE_SETS, "1".to_string()),
        (tags::QUOTE_SET_ID, "1".to_string()),
        (tags::UNDERLYING_SYMBOL, symbol.to_string()), // required in FIX 4.2
        (tags::TOT_QUOTE_ENTRIES, "1".to_string()),    // required in FIX 4.2
        (tags::NO_QUOTE_ENTRIES, "1".to_string()),
        (tags::QUOTE_ENTRY_ID, symbol.to_string()),
        (tags::SYMBOL, symbol.to_string()),
        (tags::BID_SIZE, fix_decimal(bid_quantity)),
        (tags::BID_PX, fix_decimal(bid_price)),
        (tags::OFFER_SIZE, fix_decimal(ask_quantity)),
        (tags::OFFER_PX, fix_decimal(ask_price)),
    ]
}

// ---- exchange -> initiator ----

/// mirrors sendExecutionReport / sendTradeExecutionReport in Go
/// FIX 4.2 semantics: ExecType carries the event kind - fills are 1 (Partial
/// fill) / 2 (Fill), non-fill status reports mirror the order state (0 New /
/// 4 Canceled / 8 Rejected). "F=Trade" and "I=Order Status" do not exist in 4.2.
pub fn build_execution_report(report: &Report) -> Vec<(u32, String)> {
    let (order, symbol, last_price, last_quantity) = match report {
        Report::Status { order, symbol } => (order, symbol, None, None),
        Report::Fill { order, symbol, last_price, last_quantity } => {
            (order, symbol, Some(*last_price), Some(*last_quantity))
        }
    };
    let leaves = order.remaining;
    let cum = order.quantity - order.remaining;
    let is_fill = matches!(report, Report::Fill { .. });
    let exec_type = if is_fill {
        // 4.2: 1 = Partial fill, 2 = Fill
        if leaves.is_zero() { "2" } else { "1" }
    } else {
        match order.state {
            OrderState::Cancelled => "4",
            OrderState::Rejected => "8",
            _ => "0", // New
        }
    };
    // fill reports show Partial while quantity remains, mirroring Go
    let ord_status = if is_fill && !leaves.is_zero() {
        "1"
    } else {
        ord_status_to_fix(order.state)
    };
    let mut fields = vec![
        (tags::ORDER_ID, order.exchange_id.clone()),
        (tags::EXEC_ID, order.exchange_id.clone()),
        (tags::EXEC_TRANS_TYPE, "0".to_string()), // required in FIX 4.2: New
        (tags::EXEC_TYPE, exec_type.to_string()),
        (tags::ORD_STATUS, ord_status.to_string()),
        (tags::SIDE, side_to_fix(order.side).to_string()),
        (tags::LEAVES_QTY, fix_decimal(leaves)),
        (tags::CUM_QTY, fix_decimal(cum)),
        (tags::PRICE, fix_decimal(order.price)),
        (tags::ORDER_QTY, fix_decimal(order.quantity)),
        (tags::CL_ORD_ID, order.id.to_string()),
        (tags::SYMBOL, symbol.clone()),
    ];
    if let (Some(px), Some(qty)) = (last_price, last_quantity) {
        fields.push((tags::LAST_PX, fix_decimal(px)));
        fields.push((tags::LAST_QTY, fix_decimal(qty)));
    }
    fields
}

pub fn build_security_definition(req_id: &str, symbol: &str, instrument_id: i64) -> Vec<(u32, String)> {
    vec![
        (tags::SECURITY_REQ_ID, req_id.to_string()),
        (tags::SECURITY_RESPONSE_ID, instrument_id.to_string()),
        (tags::TOTAL_NUM_SECURITIES, "1".to_string()), // required in FIX 4.2
        (tags::SECURITY_RESPONSE_TYPE, "2".to_string()),
        (tags::SYMBOL, symbol.to_string()),
        (tags::SECURITY_ID, instrument_id.to_string()),
    ]
}

pub fn build_mass_quote_ack(quote_id: &str) -> Vec<(u32, String)> {
    vec![(tags::QUOTE_STATUS, "0".to_string()), (tags::QUOTE_ID, quote_id.to_string())]
}

// ---- decode ----

#[derive(Debug)]
pub enum Inbound {
    Logon { heart_bt_int: u32 },
    Logout,
    Heartbeat,
    TestRequest { test_req_id: String },
    ResendRequest,
    NewOrderSingle { cl_ord_id: i32, symbol: String, side: Side, order_type: OrderType, price: Decimal, quantity: Decimal },
    CancelRequest { cl_ord_id: i32, orig_cl_ord_id: i32 },
    CancelReplace { cl_ord_id: i32, orig_cl_ord_id: i32, price: Decimal, quantity: Decimal },
    MassQuote { quote_id: String, ack: bool, symbol: String, bid_px: Decimal, bid_qty: Decimal, offer_px: Decimal, offer_qty: Decimal },
    SecurityDefinition { req_id: String, symbol: String, instrument_id: i64 },
    BusinessReject { reason: String },
    ExecutionReport(ExecReportData),
    Unsupported(String),
}

#[derive(Debug)]
pub struct ExecReportData {
    pub exchange_id: String,
    pub cl_ord_id: i32,
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

pub fn decode(message: &super::frame::FixMessage) -> Result<Inbound, String> {
    let msg_type = message.msg_type().ok_or("missing 35 MsgType")?;
    let get = |tag: u32| -> Option<&str> { message.get(tag) };
    let need = |tag: u32| -> Result<&str, String> {
        get(tag).ok_or_else(|| format!("missing tag {} in {}", tag, msg_type))
    };
    let num = |tag: u32| -> Result<i32, String> { need(tag)?.parse::<i32>().map_err(|e| format!("tag {}: {}", tag, e)) };
    let dec = |tag: u32| -> Result<Decimal, String> { parse_decimal(need(tag)?) };

    match msg_type {
        msg_type::LOGON => {
            let heart = get(tags::HEART_BT_INT).and_then(|v| v.parse().ok()).unwrap_or(30);
            Ok(Inbound::Logon { heart_bt_int: heart })
        }
        msg_type::LOGOUT => Ok(Inbound::Logout),
        msg_type::HEARTBEAT => Ok(Inbound::Heartbeat),
        msg_type::TEST_REQUEST => Ok(Inbound::TestRequest { test_req_id: need(tags::TEST_REQ_ID)?.to_string() }),
        msg_type::RESEND_REQUEST => Ok(Inbound::ResendRequest),
        msg_type::NEW_ORDER_SINGLE => {
            let side = side_from_fix(need(tags::SIDE)?)?;
            let order_type = match need(tags::ORD_TYPE)? {
                "1" => OrderType::Limit,
                "2" => OrderType::Market,
                other => return Err(format!("unsupported OrdType {}", other)),
            };
            Ok(Inbound::NewOrderSingle {
                cl_ord_id: num(tags::CL_ORD_ID)?,
                symbol: need(tags::SYMBOL)?.to_string(),
                side,
                order_type,
                price: get(tags::PRICE).and_then(|v| parse_decimal(v).ok()).unwrap_or(Decimal::ZERO),
                quantity: dec(tags::ORDER_QTY)?,
            })
        }
        msg_type::ORDER_CANCEL_REQUEST => {
            // the Go acceptor cancels by ClOrdID (tag 11), falling back to 41;
            // spec-style clients send a fresh 11 and point 41 at the order
            let cl_ord_id = match get(tags::CL_ORD_ID) {
                Some(v) => v.parse::<i32>().map_err(|e| format!("tag 11: {}", e))?,
                None => num(tags::ORIG_CL_ORD_ID)?,
            };
            let orig_cl_ord_id = get(tags::ORIG_CL_ORD_ID)
                .and_then(|v| v.parse().ok())
                .unwrap_or(cl_ord_id);
            Ok(Inbound::CancelRequest { cl_ord_id, orig_cl_ord_id })
        }
        msg_type::ORDER_CANCEL_REPLACE_REQUEST => {
            let cl_ord_id = num(tags::CL_ORD_ID)?;
            let orig_cl_ord_id = get(tags::ORIG_CL_ORD_ID)
                .and_then(|v| v.parse().ok())
                .unwrap_or(cl_ord_id);
            Ok(Inbound::CancelReplace { cl_ord_id, orig_cl_ord_id, price: dec(tags::PRICE)?, quantity: dec(tags::ORDER_QTY)? })
        }
        msg_type::MASS_QUOTE => {
            let sets = need(tags::NO_QUOTE_SETS)?;
            if sets != "1" {
                return Err(format!("only 1 quote set supported, got {}", sets));
            }
            let entries = need(tags::NO_QUOTE_ENTRIES)?;
            if entries != "1" {
                return Err(format!("only 1 quote supported, got {}", entries));
            }
            let ack = get(tags::QUOTE_RESPONSE_LEVEL).map(|v| v == "1").unwrap_or(false);
            Ok(Inbound::MassQuote {
                quote_id: need(tags::QUOTE_ID)?.to_string(),
                ack,
                symbol: need(tags::SYMBOL)?.to_string(),
                bid_px: dec(tags::BID_PX)?,
                bid_qty: dec(tags::BID_SIZE)?,
                offer_px: dec(tags::OFFER_PX)?,
                offer_qty: dec(tags::OFFER_SIZE)?,
            })
        }
        msg_type::SECURITY_DEFINITION => Ok(Inbound::SecurityDefinition {
            req_id: need(tags::SECURITY_REQ_ID)?.to_string(),
            instrument_id: need(tags::SECURITY_ID)?.parse().map_err(|e| format!("tag 48: {}", e))?,
            symbol: need(tags::SYMBOL)?.to_string(),
        }),
        msg_type::EXECUTION_REPORT => {
            let is_fill = matches!(need(tags::EXEC_TYPE)?, "1" | "2"); // FIX 4.2: 1=Partial fill, 2=Fill
            Ok(Inbound::ExecutionReport(ExecReportData {
                exchange_id: need(tags::ORDER_ID)?.to_string(),
                cl_ord_id: num(tags::CL_ORD_ID)?,
                symbol: need(tags::SYMBOL)?.to_string(),
                state: ord_status_from_fix(need(tags::ORD_STATUS)?)?,
                is_fill,
                side: side_from_fix(need(tags::SIDE)?)?,
                price: dec(tags::PRICE)?,
                quantity: dec(tags::ORDER_QTY)?,
                remaining: dec(tags::LEAVES_QTY)?,
                last_price: get(tags::LAST_PX).and_then(|v| parse_decimal(v).ok()).unwrap_or(Decimal::ZERO),
                last_quantity: get(tags::LAST_QTY).and_then(|v| parse_decimal(v).ok()).unwrap_or(Decimal::ZERO),
            }))
        }
        msg_type::BUSINESS_MESSAGE_REJECT => Ok(Inbound::BusinessReject {
            reason: get(tags::TEXT).unwrap_or("").to_string(),
        }),
        other => Ok(Inbound::Unsupported(other.to_string())),
    }
}

// silence unused import if fields vec not used in some builds
#[allow(unused)]
fn _assert_fields_type(fields: &[(u32, String)]) {}

#[cfg(test)]
mod tests {
    use super::super::frame;
    use super::*;

    #[test]
    fn test_new_order_single_roundtrip() {
        let fields = build_new_order_single(
            OrderIdForWire("7".to_string()),
            "IBM",
            Side::Buy,
            OrderType::Limit,
            "99.5".parse().unwrap(),
            "10".parse().unwrap(),
        );
        let bytes = frame::frame("FIX.4.2", msg_type::NEW_ORDER_SINGLE, 3, "CLIENT", "GOX", &fields);
        let mut reader = std::io::Cursor::new(bytes);
        let message = frame::read_message(&mut reader).unwrap().unwrap();
        match decode(&message).unwrap() {
            Inbound::NewOrderSingle { cl_ord_id, symbol, side, order_type, price, quantity } => {
                assert_eq!(cl_ord_id, 7);
                assert_eq!(symbol, "IBM");
                assert_eq!(side, Side::Buy);
                assert_eq!(order_type, OrderType::Limit);
                assert_eq!(price, "99.5".parse::<Decimal>().unwrap());
                assert_eq!(quantity, "10".parse::<Decimal>().unwrap());
            }
            other => panic!("unexpected {:?}", other),
        }
    }

    #[test]
    fn test_execution_report_fill() {
        let order = crate::core::order::Order {
            session_id: "s".to_string(),
            id: 5,
            exchange_id: "12".to_string(),
            instrument_id: 1,
            side: Side::Buy,
            order_type: OrderType::Limit,
            price: "100".parse().unwrap(),
            quantity: "10".parse().unwrap(),
            remaining: "4".parse().unwrap(),
            state: OrderState::PartialFill,
            arrival: 1,
        };
        let report = Report::Fill { order, symbol: "IBM".to_string(), last_price: "100".parse().unwrap(), last_quantity: "6".parse().unwrap() };
        let fields = build_execution_report(&report);
        let bytes = frame::frame("FIX.4.2", msg_type::EXECUTION_REPORT, 9, "GOX", "CLIENT", &fields);
        let wire = String::from_utf8(bytes.clone()).unwrap();
        let mut reader = std::io::Cursor::new(bytes);
        let message = frame::read_message(&mut reader).unwrap().unwrap();
        match decode(&message).unwrap() {
            Inbound::ExecutionReport(data) => {
                assert!(data.is_fill);
                assert_eq!(data.cl_ord_id, 5);
                assert_eq!(data.remaining, "4".parse::<Decimal>().unwrap());
                assert_eq!(data.last_quantity, "6".parse::<Decimal>().unwrap());
                assert_eq!(data.state, OrderState::PartialFill);
                assert_eq!(data.exchange_id, "12");
            }
            other => panic!("unexpected {:?}", other),
        }
        // FIX 4.2 wire values: 150=1 (Partial fill) and 20=0 (ExecTransType New)
        let text = wire;
        assert!(text.contains("150=1"), "ExecType must be 1 on partial fill");
        assert!(text.contains("20=0"), "ExecTransType must be 0");
    }

    #[test]
    fn test_mass_quote_roundtrip() {
        let fields = build_mass_quote("IBM", "99.75".parse().unwrap(), "10".parse().unwrap(), "100".parse().unwrap(), "10".parse().unwrap());
        let bytes = frame::frame("FIX.4.2", msg_type::MASS_QUOTE, 4, "MM", "GOX", &fields);
        let mut reader = std::io::Cursor::new(bytes);
        let message = frame::read_message(&mut reader).unwrap().unwrap();
        match decode(&message).unwrap() {
            Inbound::MassQuote { quote_id, ack, symbol, bid_px, bid_qty, offer_px, offer_qty } => {
                assert_eq!(quote_id, "1");
                assert!(!ack);
                assert_eq!(symbol, "IBM");
                assert_eq!(bid_px, "99.75".parse::<Decimal>().unwrap());
                assert_eq!(bid_qty, "10".parse::<Decimal>().unwrap());
                assert_eq!(offer_px, "100".parse::<Decimal>().unwrap());
                assert_eq!(offer_qty, "10".parse::<Decimal>().unwrap());
            }
            other => panic!("unexpected {:?}", other),
        }
    }
}

#[test]
fn test_fix42_exectype_mapping() {
    use crate::core::exchange::NewOrder;
    let mk = |state| crate::core::order::Order {
        session_id: "s".into(),
        id: 1,
        exchange_id: "9".into(),
        instrument_id: 1,
        side: Side::Buy,
        order_type: OrderType::Limit,
        price: "100".parse().unwrap(),
        quantity: "10".parse().unwrap(),
        remaining: "10".parse().unwrap(),
        state,
        arrival: 1,
    };
    // booked -> ExecType 0 (New)
    let fields = build_execution_report(&Report::Status { order: mk(OrderState::Booked), symbol: "IBM".into() });
    assert!(fields.contains(&(tags::EXEC_TYPE, "0".to_string())));
    // cancelled -> ExecType 4
    let mut o = mk(OrderState::Cancelled);
    o.remaining = Decimal::ZERO;
    let fields = build_execution_report(&Report::Status { order: o.clone(), symbol: "IBM".into() });
    assert!(fields.contains(&(tags::EXEC_TYPE, "4".to_string())));
    assert!(fields.contains(&(tags::ORD_STATUS, "4".to_string())));
    // partial fill -> ExecType 1 with OrdStatus 1
    let mut o = mk(OrderState::PartialFill);
    o.remaining = "4".parse().unwrap();
    let fields = build_execution_report(&Report::Fill {
        order: o.clone(), symbol: "IBM".into(), last_price: "100".parse().unwrap(), last_quantity: "6".parse().unwrap(),
    });
    assert!(fields.contains(&(tags::EXEC_TYPE, "1".to_string())));
    // full fill -> ExecType 2 with OrdStatus 2
    let mut o = mk(OrderState::Filled);
    o.remaining = Decimal::ZERO;
    let fields = build_execution_report(&Report::Fill {
        order: o.clone(), symbol: "IBM".into(), last_price: "100".parse().unwrap(), last_quantity: "10".parse().unwrap(),
    });
    assert!(fields.contains(&(tags::EXEC_TYPE, "2".to_string())));
    assert!(fields.contains(&(tags::ORD_STATUS, "2".to_string())));
}
