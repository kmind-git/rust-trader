use rust_trader::core::exchange::{Engine, NewOrder, Report};
use rust_trader::core::order::{OrderType, Side};
use rust_trader::market_data::MarketDataReader;
use rust_trader::queue;
use rust_decimal::Decimal;
use std::hint::black_box;
use std::sync::{mpsc::Receiver, Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SYMBOL: &str = "BENCH";
const BOOK_ORDERS: i32 = 10_000;
const DISCONNECT_ORDERS: i32 = 8_000;
const PRICE_LEVELS: i32 = 512;
const READS_PER_READER: usize = 25_000;
const MEASURED_RUNS: usize = 5;

fn drain_reports(receiver: &Receiver<Report>) {
    while receiver.try_recv().is_ok() {}
}

fn build_engine(order_count: i32) -> (Engine, Receiver<Report>, i64) {
    let mut engine = Engine::new();
    let instrument_id = engine.create_instrument(SYMBOL);
    let (sender, receiver) = queue::channel::<Report>();
    engine.register_session("bench", sender);

    for id in 1..=order_count {
        // All orders are bids below the ask, so setup creates a stable final
        // book without matching or timing any network/report work.
        let price = Decimal::from(100_000 - id % PRICE_LEVELS);
        engine
            .create_order(
                "bench",
                NewOrder {
                    id,
                    instrument_id,
                    side: Side::Buy,
                    order_type: OrderType::Limit,
                    price,
                    quantity: Decimal::ONE,
                },
            )
            .expect("benchmark setup order");
        // The queue is bounded; this makes setup independent of its capacity.
        drain_reports(&receiver);
    }
    (engine, receiver, instrument_id)
}

fn run_old_reads(
    engine: &Arc<Mutex<Engine>>,
    readers: usize,
    reads_per_reader: usize,
) -> (Duration, u64) {
    assert!(readers > 0);
    let start_barrier = Arc::new(Barrier::new(readers + 1));
    let mut workers = Vec::with_capacity(readers);
    for _ in 0..readers {
        let engine = Arc::clone(engine);
        let start_barrier = Arc::clone(&start_barrier);
        workers.push(thread::spawn(move || {
            start_barrier.wait();
            let mut observed = 0_u64;
            for _ in 0..reads_per_reader {
                let book = engine
                    .lock()
                    .expect("old engine mutex")
                    .book(SYMBOL)
                    .expect("old book snapshot");
                observed = observed
                    .wrapping_add(book.sequence)
                    .wrapping_add(book.bids.len() as u64)
                    .wrapping_add(book.asks.len() as u64);
                black_box(&book);
            }
            black_box(observed)
        }));
    }

    let started = Instant::now();
    start_barrier.wait();
    let mut observed = 0_u64;
    for worker in workers {
        observed = observed.wrapping_add(worker.join().expect("old reader"));
    }
    (started.elapsed(), black_box(observed))
}

fn run_new_reads(
    reader: &MarketDataReader,
    readers: usize,
    reads_per_reader: usize,
) -> (Duration, u64) {
    assert!(readers > 0);
    let start_barrier = Arc::new(Barrier::new(readers + 1));
    let mut workers = Vec::with_capacity(readers);
    for _ in 0..readers {
        let reader = reader.clone();
        let start_barrier = Arc::clone(&start_barrier);
        workers.push(thread::spawn(move || {
            start_barrier.wait();
            let mut observed = 0_u64;
            for _ in 0..reads_per_reader {
                let snapshot = reader.snapshot(SYMBOL).expect("new market-data snapshot");
                observed = observed
                    .wrapping_add(snapshot.version)
                    .wrapping_add(snapshot.book.sequence)
                    .wrapping_add(snapshot.book.bids.len() as u64)
                    .wrapping_add(snapshot.book.asks.len() as u64);
                black_box(&snapshot);
            }
            black_box(observed)
        }));
    }

    let started = Instant::now();
    start_barrier.wait();
    let mut observed = 0_u64;
    for worker in workers {
        observed = observed.wrapping_add(worker.join().expect("new reader"));
    }
    (started.elapsed(), black_box(observed))
}

fn print_read_row(mode: &str, readers: usize, run: usize, elapsed: Duration, observed: u64) {
    println!(
        "read,{mode},{readers},{run},{},{observed}",
        elapsed.as_nanos()
    );
}

fn main() {
    println!(
        "# raw benchmark output; run with cargo run --release --example market_snapshot_bench"
    );
    println!("# old = Arc<Mutex<Engine>> + Engine::book deep clone");
    println!("# new = MarketDataReader::snapshot Arc read");
    println!("# each row includes barrier wakeup/join overhead and {READS_PER_READER} reads per reader; worker creation is outside timing");
    println!("# both modes use this implementation's same final Engine; this is not a full old/new release comparison or an HTTP benchmark");
    println!("# no latency, throughput, or speedup guarantee is inferred from these samples");
    println!("kind,mode,readers,run,elapsed_ns,observed");

    let (engine, _reports, instrument_id) = build_engine(BOOK_ORDERS);
    let reader = engine.market_data();
    let shared_engine = Arc::new(Mutex::new(engine));
    let old_levels = shared_engine
        .lock()
        .expect("final engine mutex")
        .book(SYMBOL)
        .expect("final old book");
    let new_snapshot = reader.snapshot(SYMBOL).expect("final new snapshot");
    assert_eq!(old_levels.instrument_id, instrument_id);
    assert_eq!(old_levels.sequence, new_snapshot.book.sequence);
    assert_eq!(old_levels.bids.len(), new_snapshot.book.bids.len());
    assert_eq!(old_levels.asks.len(), new_snapshot.book.asks.len());
    println!(
        "# final_book instrument_id={} sequence={} bid_levels={} ask_levels={}",
        instrument_id,
        new_snapshot.book.sequence,
        new_snapshot.book.bids.len(),
        new_snapshot.book.asks.len()
    );

    // One warmup per mode reduces first-use noise. Warmups are deliberately
    // omitted from the raw rows.
    for readers in [1, 4] {
        let _ = run_old_reads(&shared_engine, readers, READS_PER_READER / 4);
        let _ = run_new_reads(&reader, readers, READS_PER_READER / 4);
        for run in 1..=MEASURED_RUNS {
            let modes = if run % 2 == 1 {
                ["old", "new"]
            } else {
                ["new", "old"]
            };
            for mode in modes {
                let (elapsed, observed) = if mode == "old" {
                    run_old_reads(&shared_engine, readers, READS_PER_READER)
                } else {
                    run_new_reads(&reader, readers, READS_PER_READER)
                };
                print_read_row(mode, readers, run, elapsed, observed);
            }
        }
    }

    // This is a single implementation cost measurement for a large
    // disconnect. It is intentionally not described as an old/new comparison.
    let (mut disconnect_engine, _disconnect_reports, _) = build_engine(DISCONNECT_ORDERS);
    let started = Instant::now();
    disconnect_engine.session_disconnect("bench");
    let elapsed = started.elapsed();
    let final_book = disconnect_engine
        .book(SYMBOL)
        .expect("book remains queryable after disconnect");
    black_box(final_book);
    println!(
        "disconnect,engine,1,{DISCONNECT_ORDERS},{},0",
        elapsed.as_nanos()
    );
}
