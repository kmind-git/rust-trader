use rust_decimal::Decimal;

pub type OrderId = i32;

pub const MARKET_BUY_PRICE: i64 = 9999999999999;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_str(&self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderType {
    Market,
    Limit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderState {
    New,
    Booked,
    PartialFill,
    Filled,
    Cancelled,
    Rejected,
}

impl OrderState {
    /// mirrors common.Order.IsActive in the Go implementation
    pub fn is_active(&self) -> bool {
        !matches!(self, OrderState::Filled | OrderState::Cancelled | OrderState::Rejected)
    }
}

/// A limit or market order. The exchange engine owns orders; `session_id` and
/// `id` (the client ClOrdID) uniquely locate an order. Quote legs use `id = 0`.
#[derive(Clone, Debug)]
pub struct Order {
    pub session_id: String,
    pub id: OrderId,
    pub exchange_id: String,
    pub instrument_id: i64,
    pub side: Side,
    pub order_type: OrderType,
    pub price: Decimal,
    pub quantity: Decimal,
    pub remaining: Decimal,
    pub state: OrderState,
    /// global monotonic arrival counter; replaces Go's time.Now() ordering and
    /// gives deterministic FIFO priority within a price level
    pub arrival: u64,
}

impl Order {
    pub fn limit(
        session_id: &str,
        id: OrderId,
        instrument_id: i64,
        side: Side,
        price: Decimal,
        quantity: Decimal,
        arrival: u64,
    ) -> Order {
        Order {
            session_id: session_id.to_string(),
            id,
            exchange_id: String::new(),
            instrument_id,
            side,
            order_type: OrderType::Limit,
            price,
            quantity,
            remaining: quantity,
            state: OrderState::New,
            arrival,
        }
    }

    pub fn market(
        session_id: &str,
        id: OrderId,
        instrument_id: i64,
        side: Side,
        quantity: Decimal,
        arrival: u64,
    ) -> Order {
        Order {
            price: Decimal::ZERO,
            order_type: OrderType::Market,
            ..Order::limit(session_id, id, instrument_id, side, Decimal::ZERO, quantity, arrival)
        }
    }

    /// the "effective price" used for ordering, so market orders always sit at
    /// the top of their side (mirrors sessionOrder.getPrice in Go)
    pub fn effective_price(&self) -> Decimal {
        match self.order_type {
            OrderType::Market => match self.side {
                Side::Buy => Decimal::from(MARKET_BUY_PRICE),
                Side::Sell => Decimal::ZERO,
            },
            OrderType::Limit => self.price,
        }
    }
}

pub fn min_decimal(a: Decimal, b: Decimal) -> Decimal {
    if a < b {
        a
    } else {
        b
    }
}
