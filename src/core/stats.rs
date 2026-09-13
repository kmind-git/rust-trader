use rust_decimal::Decimal;

/// running per-instrument statistics, updated synchronously by the matching
/// path (mirrors the Statistics struct in Go's marketdata.go, now in stats.go)
#[derive(Clone, Debug, Default)]
pub struct Statistics {
    pub symbol: String,
    pub bid_qty: Decimal,
    pub bid_price: Decimal,
    pub ask_qty: Decimal,
    pub ask_price: Decimal,
    pub volume: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub has_high_low: bool,
}
