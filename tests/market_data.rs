use gotrader::core::exchange::{Engine, Report};
use gotrader::market_data::InstrumentSnapshot;
use gotrader::queue;
use rust_decimal::Decimal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Receiver, Arc, Barrier};
use std::thread;

fn decimal(value: i64) -> Decimal {
    Decimal::from(value)
}

fn drain_reports(receiver: &Receiver<Report>) {
    while receiver.try_recv().is_ok() {}
}

fn assert_snapshot_consistent(snapshot: &InstrumentSnapshot) {
    assert_eq!(
        snapshot.version, snapshot.book.sequence,
        "the public version and book sequence must identify one publication"
    );

    match snapshot.book.bids.first() {
        Some(level) => {
            assert_eq!(snapshot.stats.bid_price, level.price);
            assert_eq!(snapshot.stats.bid_qty, level.quantity);
        }
        None => {
            assert_eq!(snapshot.stats.bid_price, Decimal::ZERO);
            assert_eq!(snapshot.stats.bid_qty, Decimal::ZERO);
        }
    }
    match snapshot.book.asks.first() {
        Some(level) => {
            assert_eq!(snapshot.stats.ask_price, level.price);
            assert_eq!(snapshot.stats.ask_qty, level.quantity);
        }
        None => {
            assert_eq!(snapshot.stats.ask_price, Decimal::ZERO);
            assert_eq!(snapshot.stats.ask_qty, Decimal::ZERO);
        }
    }
}

fn snapshot_is_consistent(snapshot: &InstrumentSnapshot) -> bool {
    if snapshot.version != snapshot.book.sequence {
        return false;
    }
    match snapshot.book.bids.first() {
        Some(level)
            if snapshot.stats.bid_price == level.price
                && snapshot.stats.bid_qty == level.quantity => {}
        Some(_) => return false,
        None if snapshot.stats.bid_price != Decimal::ZERO
            || snapshot.stats.bid_qty != Decimal::ZERO =>
        {
            return false;
        }
        None => {}
    }
    match snapshot.book.asks.first() {
        Some(level)
            if snapshot.stats.ask_price == level.price
                && snapshot.stats.ask_qty == level.quantity => {}
        Some(_) => return false,
        None if snapshot.stats.ask_price != Decimal::ZERO
            || snapshot.stats.ask_qty != Decimal::ZERO =>
        {
            return false;
        }
        None => {}
    }
    true
}

#[test]
fn reader_sees_instruments_created_after_reader_creation() {
    let mut engine = Engine::new();
    let reader = engine.market_data();

    assert!(reader.snapshot("IBM").is_none());
    assert!(reader.all_symbols().is_empty());

    let instrument_id = engine.create_instrument("IBM");
    let snapshot = reader
        .snapshot("IBM")
        .expect("a known, untraded instrument has an empty snapshot");

    assert_eq!(snapshot.version, 0);
    assert_eq!(snapshot.book.sequence, 0);
    assert_eq!(snapshot.book.instrument_id, instrument_id);
    assert!(snapshot.book.bids.is_empty());
    assert!(snapshot.book.asks.is_empty());
    assert_snapshot_consistent(&snapshot);
    assert_eq!(reader.all_symbols(), vec!["IBM".to_string()]);
}

#[test]
fn old_snapshot_arcs_remain_stable_after_update_and_disconnect() {
    let mut engine = Engine::new();
    let instrument_id = engine.create_instrument("IBM");
    let (sender, receiver) = queue::channel::<Report>();
    engine.register_session("maker", sender);
    let reader = engine.market_data();

    engine
        .quote(
            "maker",
            instrument_id,
            decimal(100),
            decimal(10),
            decimal(101),
            decimal(10),
        )
        .unwrap();
    drain_reports(&receiver);
    let first = reader.snapshot("IBM").expect("first quote publication");
    assert_snapshot_consistent(&first);
    assert_eq!(first.book.bids[0].price, decimal(100));
    assert_eq!(first.book.bids[0].quantity, decimal(10));
    assert_eq!(first.book.asks[0].price, decimal(101));

    engine
        .quote(
            "maker",
            instrument_id,
            decimal(99),
            decimal(4),
            decimal(102),
            decimal(6),
        )
        .unwrap();
    drain_reports(&receiver);
    let second = reader.snapshot("IBM").expect("replacement publication");
    assert_snapshot_consistent(&second);
    assert!(second.version > first.version);
    assert_eq!(second.book.bids[0].price, decimal(99));
    assert_eq!(second.book.bids[0].quantity, decimal(4));
    assert_eq!(second.book.asks[0].price, decimal(102));

    engine.session_disconnect("maker");
    drain_reports(&receiver);
    let after_disconnect = reader
        .snapshot("IBM")
        .expect("disconnect publishes the final empty state");
    assert_snapshot_consistent(&after_disconnect);
    assert!(after_disconnect.version > second.version);
    assert!(after_disconnect.book.bids.is_empty());
    assert!(after_disconnect.book.asks.is_empty());

    // ArcSwap-style publication must not mutate data already handed to a
    // caller.  These checks also keep this test independent of pointer reuse.
    drop(engine);
    assert_eq!(first.book.bids[0].price, decimal(100));
    assert_eq!(first.book.bids[0].quantity, decimal(10));
    assert_eq!(first.book.asks[0].price, decimal(101));
    assert_eq!(second.book.bids[0].price, decimal(99));
    assert_eq!(second.book.bids[0].quantity, decimal(4));
    assert_eq!(second.book.asks[0].price, decimal(102));
}

#[test]
fn concurrent_readers_see_consistent_monotonic_snapshots() {
    const READER_COUNT: usize = 4;
    const PUBLICATIONS: i64 = 400;

    let mut engine = Engine::new();
    let instrument_id = engine.create_instrument("IBM");
    let (sender, receiver) = queue::channel::<Report>();
    engine.register_session("writer", sender);
    let reader = engine.market_data();
    let start = Arc::new(Barrier::new(READER_COUNT + 1));
    let stop = Arc::new(AtomicBool::new(false));
    let failed = Arc::new(AtomicBool::new(false));

    let mut readers = Vec::with_capacity(READER_COUNT);
    for _ in 0..READER_COUNT {
        let reader = reader.clone();
        let start = start.clone();
        let stop = stop.clone();
        let failed = failed.clone();
        readers.push(thread::spawn(move || {
            start.wait();
            let mut previous_version = 0;
            while !stop.load(Ordering::Acquire) {
                let Some(snapshot) = reader.snapshot("IBM") else {
                    failed.store(true, Ordering::Release);
                    break;
                };
                if snapshot.version < previous_version {
                    failed.store(true, Ordering::Release);
                    break;
                }
                if !snapshot_is_consistent(&snapshot) {
                    failed.store(true, Ordering::Release);
                    break;
                }
                previous_version = snapshot.version;
                thread::yield_now();
            }
        }));
    }

    let writer_start = start.clone();
    let writer_stop = stop.clone();
    let writer_failed = failed.clone();
    let writer = thread::spawn(move || {
        writer_start.wait();
        for index in 0..PUBLICATIONS {
            let bid_price = decimal(100 + index % 7);
            let ask_price = bid_price + Decimal::ONE;
            if engine
                .quote(
                    "writer",
                    instrument_id,
                    bid_price,
                    decimal(1 + index % 11),
                    ask_price,
                    decimal(1 + index % 13),
                )
                .is_err()
            {
                writer_failed.store(true, Ordering::Release);
                break;
            }
            // Quotes do not normally generate reports, but draining on every
            // write keeps this test safe if that implementation detail changes.
            drain_reports(&receiver);
            thread::yield_now();
        }
        writer_stop.store(true, Ordering::Release);
        // Dropping the engine here verifies that readers own the publication
        // state rather than borrowing the mutable engine.
    });

    writer.join().unwrap();
    for reader in readers {
        reader.join().unwrap();
    }
    assert!(!failed.load(Ordering::Acquire));

    // No follow-up trade or write is needed to retrieve the last publication.
    let final_snapshot = reader
        .snapshot("IBM")
        .expect("the latest snapshot remains readable after the writer exits");
    assert_snapshot_consistent(&final_snapshot);
    assert_eq!(final_snapshot.version, PUBLICATIONS as u64);
    let last = PUBLICATIONS - 1;
    assert_eq!(final_snapshot.book.bids[0].price, decimal(100 + last % 7));
    assert_eq!(final_snapshot.book.bids[0].quantity, decimal(1 + last % 11));
    assert_eq!(final_snapshot.book.asks[0].quantity, decimal(1 + last % 13));
}
