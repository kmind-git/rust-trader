use std::sync::mpsc;

use rust_decimal::Decimal;

use super::*;
use crate::core::order::{OrderState, OrderType, Side};

/// translated from exchange_test.go TestExchangeLevelBasics
#[test]
fn test_exchange_level_basics() {
    let mut engine = Engine::new();
    let (tx1, rx1) = mpsc::channel();
    let (tx2, rx2) = mpsc::channel();
    engine.register_session("client1", tx1);
    engine.register_session("client2", tx2);

    let inst_id = engine.create_instrument("AAPL");

    // 1. submit an order
    let oid1 = engine
        .create_order(
            "client1",
            NewOrder {
                id: 1,
                instrument_id: inst_id,
                side: Side::Buy,
                order_type: OrderType::Limit,
                price: "150.00".parse().unwrap(),
                quantity: "100".parse().unwrap(),
            },
        )
        .unwrap();
    assert_eq!(oid1, 1);

    let order1 = engine.sessions["client1"].orders.get(&1).unwrap().clone();
    assert_eq!(order1.state, OrderState::Booked);

    // 2. modify the order
    engine.modify_order("client1", oid1, "151.00".parse().unwrap(), "100".parse().unwrap()).unwrap();
    let order1 = engine.sessions["client1"].orders.get(&1).unwrap().clone();
    assert_eq!(order1.price, "151".parse().unwrap(), "expected modified price 151");

    // 3. cross an order to generate a fill
    engine
        .create_order(
            "client2",
            NewOrder {
                id: 2,
                instrument_id: inst_id,
                side: Side::Sell,
                order_type: OrderType::Limit,
                price: "150.00".parse().unwrap(),
                quantity: "50".parse().unwrap(),
            },
        )
        .unwrap();

    // order2 fully filled
    let order2 = engine.sessions["client2"].orders.get(&2).unwrap().clone();
    assert_eq!(order2.state, OrderState::Filled);

    // order1 partially filled
    let order1 = engine.sessions["client1"].orders.get(&1).unwrap().clone();
    assert_eq!(order1.state, OrderState::PartialFill);
    assert_eq!(order1.remaining, "50".parse().unwrap());

    // 4. modifying an already filled order fails
    let err = engine.modify_order("client2", 2, "150.00".parse().unwrap(), "10".parse().unwrap());
    assert_eq!(err, Err(EngineError::OrderIsNotActive));

    // 5. cross another order to fully fill the resting order
    engine
        .create_order(
            "client2",
            NewOrder {
                id: 3,
                instrument_id: inst_id,
                side: Side::Sell,
                order_type: OrderType::Limit,
                price: "151.00".parse().unwrap(),
                quantity: "50".parse().unwrap(),
            },
        )
        .unwrap();

    let order1 = engine.sessions["client1"].orders.get(&1).unwrap().clone();
    let order3 = engine.sessions["client2"].orders.get(&3).unwrap().clone();
    assert_eq!(order1.state, OrderState::Filled);
    assert_eq!(order3.state, OrderState::Filled);

    // modifying the now filled resting order fails
    let err = engine.modify_order("client1", 1, "152.00".parse().unwrap(), "100".parse().unwrap());
    assert_eq!(err, Err(EngineError::OrderIsNotActive));

    // report flow assertions (beyond the Go test: verifies the report wiring)
    let mut c1_status = 0;
    let mut c1_fill = 0;
    for report in rx1.try_iter() {
        match report {
            Report::Status { .. } => c1_status += 1,
            Report::Fill { .. } => c1_fill += 1,
        }
    }
    // order1: booked status on create, booked status on modify (no trades),
    // then 2 fills (partial, full)
    assert_eq!(c1_status, 2, "client1 status reports");
    assert_eq!(c1_fill, 2, "client1 fill reports");

    let mut c2_fill = 0;
    for report in rx2.try_iter() {
        match report {
            Report::Status { .. } => panic!("unexpected status report for taker"),
            Report::Fill { .. } => c2_fill += 1,
        }
    }
    assert_eq!(c2_fill, 2, "client2 fill reports");
}

#[test]
fn test_market_order_remainder_cancelled() {
    let mut engine = Engine::new();
    let (tx1, rx1) = mpsc::channel();
    engine.register_session("client1", tx1);
    let inst_id = engine.create_instrument("IBM");
    engine
        .create_order(
            "client1",
            NewOrder {
                id: 1,
                instrument_id: inst_id,
                side: Side::Buy,
                order_type: OrderType::Limit,
                price: "100".parse().unwrap(),
                quantity: "10".parse().unwrap(),
            },
        )
        .unwrap();
    // market sell 25: fills 10, remainder cancelled
    engine
        .create_order(
            "client1",
            NewOrder {
                id: 2,
                instrument_id: inst_id,
                side: Side::Sell,
                order_type: OrderType::Market,
                price: Decimal::ZERO,
                quantity: "25".parse().unwrap(),
            },
        )
        .unwrap();

    let order2 = engine.sessions["client1"].orders.get(&2).unwrap().clone();
    assert_eq!(order2.state, OrderState::Cancelled);
    assert_eq!(order2.remaining, "15".parse().unwrap());

    let stats = engine.statistics("IBM").unwrap();
    assert_eq!(stats.volume, "10".parse().unwrap());
    assert!(stats.has_high_low);
    drop(rx1);
}

#[test]
fn test_modify_not_found_and_quote_replace() {
    let mut engine = Engine::new();
    let (tx1, _rx1) = mpsc::channel();
    engine.register_session("mm", tx1);
    let inst_id = engine.create_instrument("IBM");

    // modify unknown order
    let err = engine.modify_order("mm", 99, "1".parse().unwrap(), "1".parse().unwrap());
    assert_eq!(err, Err(EngineError::OrderNotFound));

    // quote, then re-quote replaces the pair
    engine
        .quote(
            "mm",
            inst_id,
            "99".parse().unwrap(),
            "10".parse().unwrap(),
            "101".parse().unwrap(),
            "10".parse().unwrap(),
        )
        .unwrap();
    engine
        .quote(
            "mm",
            inst_id,
            "98".parse().unwrap(),
            "10".parse().unwrap(),
            "102".parse().unwrap(),
            "10".parse().unwrap(),
        )
        .unwrap();
    let book = engine.book("IBM").unwrap();
    assert_eq!(book.bids.len(), 1);
    assert_eq!(book.bids[0].price, "98".parse().unwrap());
    assert_eq!(book.asks[0].price, "102".parse().unwrap());

    // session disconnect withdraws everything
    engine.session_disconnect("mm");
    let book = engine.book("IBM").unwrap();
    assert_eq!(book.bids.len(), 0);
    assert_eq!(book.asks.len(), 0);
    assert!(engine.session_ids().is_empty());
}

#[test]
fn test_sequence_and_stats() {
    let mut engine = Engine::new();
    let (tx1, _rx1) = mpsc::channel();
    engine.register_session("s", tx1);
    let inst_id = engine.create_instrument("IBM");
    engine
        .quote(
            "s",
            inst_id,
            "100".parse().unwrap(),
            "5".parse().unwrap(),
            "101".parse().unwrap(),
            "5".parse().unwrap(),
        )
        .unwrap();
    let book = engine.book("IBM").unwrap();
    assert_eq!(book.sequence, 1);
    engine
        .create_order(
            "s",
            NewOrder {
                id: 1,
                instrument_id: inst_id,
                side: Side::Buy,
                order_type: OrderType::Market,
                price: Decimal::ZERO,
                quantity: "2".parse().unwrap(),
            },
        )
        .unwrap();
    let book = engine.book("IBM").unwrap();
    assert_eq!(book.sequence, 2);
    let stats = engine.statistics("IBM").unwrap();
    assert_eq!(stats.volume, "2".parse().unwrap());
    assert_eq!(stats.high, "101".parse().unwrap());
    assert_eq!(stats.low, "101".parse().unwrap());
}

#[test]
fn test_market_partial_fill_report_sequence() {
    // 市价单部分成交 + 剩余撤销：Fill(Partial) 在前，Status(Cancelled) 在后
    let mut engine = Engine::new();
    let (tx, rx) = mpsc::channel();
    engine.register_session("c", tx);
    let inst_id = engine.create_instrument("IBM");
    engine
        .create_order(
            "c",
            NewOrder {
                id: 1,
                instrument_id: inst_id,
                side: Side::Buy,
                order_type: OrderType::Limit,
                price: "100".parse().unwrap(),
                quantity: "10".parse().unwrap(),
            },
        )
        .unwrap();
    engine
        .create_order(
            "c",
            NewOrder {
                id: 2,
                instrument_id: inst_id,
                side: Side::Sell,
                order_type: OrderType::Market,
                price: Decimal::ZERO,
                quantity: "25".parse().unwrap(),
            },
        )
        .unwrap();

    let seq: Vec<String> = rx
        .try_iter()
        .map(|r| match r {
            Report::Fill { order, .. } => format!("Fill {} remaining {}", order.id, order.remaining),
            Report::Status { order, .. } => format!("Status {} {:?}", order.id, order.state),
        })
        .collect();
    assert_eq!(
        seq,
        vec![
            "Status 1 Booked",        // 限价单挂上簿，无成交 → 状态确认
            "Fill 1 remaining 0",     // 市价单吃光限价单：卖方成交回报
            "Fill 2 remaining 15",    // 市价单自身成交 10，剩 15 → Partial
            "Status 2 Cancelled",     // 市价单剩余数量撤销 → 终态回报
        ],
        "report sequence: {:?}",
        seq
    );
}
