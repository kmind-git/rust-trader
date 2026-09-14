use gotrader::core::{
    order::{Order, Side},
    orderbook::OrderBook,
};
use rust_decimal::Decimal;
use std::{hint::black_box, time::Instant};
fn main() {
    for levels in [1, 100, 2000] {
        let mut book = OrderBook::new(1);
        let start = Instant::now();
        for id in 1..=20000 {
            black_box(book.add(Order::limit(
                "bench",
                id,
                1,
                Side::Buy,
                Decimal::from(1 + id % levels),
                Decimal::ONE,
                id as u64,
            )));
        }
        let insert = start.elapsed();
        let start = Instant::now();
        for _ in 0..500 {
            black_box(book.build_book());
        }
        let snapshot = start.elapsed();
        let start = Instant::now();
        // Reverse order stresses cancellation away from the FIFO head.
        for id in (1..=20000).rev() {
            black_box(
                book.remove("bench", id, Side::Buy, Decimal::from(1 + id % levels))
                    .unwrap(),
            );
        }
        println!(
            "levels={levels} insert_ms={:.3} snapshot500_ms={:.3} cancel_ms={:.3}",
            insert.as_secs_f64() * 1000.,
            snapshot.as_secs_f64() * 1000.,
            start.elapsed().as_secs_f64() * 1000.
        );
    }
}
