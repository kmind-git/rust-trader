use std::collections::{BTreeMap, HashMap};
use std::time::SystemTime;

use rust_decimal::Decimal;

use super::order::{min_decimal, Order, OrderId, OrderState, OrderType, Side};

/// aggregated price level for book snapshots (mirrors common.BookLevel)
#[derive(Clone, Debug)]
pub struct BookLevel {
    pub price: Decimal,
    pub quantity: Decimal,
}

/// full book snapshot (mirrors common.Book; symbol resolved by callers)
#[derive(Clone, Debug)]
pub struct Book {
    pub instrument_id: i64,
    pub sequence: u64,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

impl Book {
    pub fn has_bids(&self) -> bool {
        !self.bids.is_empty()
    }
    pub fn has_asks(&self) -> bool {
        !self.asks.is_empty()
    }
}

/// a trade produced by matching, with post-fill snapshots of both sides
#[derive(Clone, Debug)]
pub struct RawTrade {
    pub buyer: Order,
    pub seller: Order,
    pub price: Decimal,
    pub quantity: Decimal,
    pub trade_id: i64,
    pub when: SystemTime,
}

#[derive(Debug)]
struct PriceLevel {
    price: Decimal,
    orders: BTreeMap<i64, Order>,
    index: HashMap<(String, OrderId), i64>,
    total: Decimal,
    next: i64,
}

impl PriceLevel {
    fn new(price: Decimal) -> Self {
        Self {
            price,
            orders: BTreeMap::new(),
            index: HashMap::new(),
            total: Decimal::ZERO,
            next: 0,
        }
    }
    fn top(&self) -> &Order {
        self.orders.first_key_value().unwrap().1
    }
    fn push_back(&mut self, order: Order) {
        let key = self.next;
        self.next = self
            .next
            .checked_add(1)
            .expect("price level priority exhausted");
        self.insert_at(key, order);
    }
    fn insert_at(&mut self, key: i64, order: Order) {
        let identity = (order.session_id.clone(), order.id);
        assert!(
            !self.index.contains_key(&identity),
            "duplicate order in price level"
        );
        self.total += order.remaining;
        self.index.insert(identity, key);
        self.orders.insert(key, order);
    }
    #[cfg(test)]
    fn push_front(&mut self, order: Order) {
        let key = self
            .orders
            .first_key_value()
            .map(|(k, _)| *k - 1)
            .unwrap_or(-1);
        self.insert_at(key, order);
    }
    fn locate_mut(&mut self, session: &str, id: OrderId) -> Option<&mut Order> {
        let key = self.index.get(&(session.to_string(), id))?;
        self.orders.get_mut(key)
    }
    fn remove(&mut self, session: &str, id: OrderId) -> Result<Order, ()> {
        let key = self.index.remove(&(session.to_string(), id)).ok_or(())?;
        let order = self.orders.remove(&key).unwrap();
        self.total -= order.remaining;
        Ok(order)
    }
    fn update_fill(&mut self, order: &Order, quantity: Decimal) {
        if order.remaining.is_zero() {
            self.remove(&order.session_id, order.id).unwrap();
        } else {
            self.total -= quantity;
            *self.locate_mut(&order.session_id, order.id).unwrap() = order.clone();
        }
    }
}

/// the order book for one instrument: descending bid levels, ascending ask
/// levels, FIFO (price-time priority) within a level.
#[derive(Debug)]
pub struct OrderBook {
    pub instrument_id: i64,
    bids: BTreeMap<Decimal, PriceLevel>,
    asks: BTreeMap<Decimal, PriceLevel>,
}

impl OrderBook {
    pub fn new(instrument_id: i64) -> OrderBook {
        OrderBook {
            instrument_id,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
        }
    }

    /// insert an order (state set to Booked), run matching, and cancel any
    /// unfilled remainder of a market order. Returns the trades plus the final
    /// state of the incoming order (None if it cannot be determined, which
    /// does not happen with current matching semantics). Mirrors orderBook.add.
    pub fn add(&mut self, mut order: Order) -> (Vec<RawTrade>, Option<Order>) {
        order.state = OrderState::Booked;
        self.insert(&order);
        let trades = self.match_trades();

        let mut final_state = None;
        match self.locate_mut(
            &order.session_id,
            order.id,
            order.side,
            order.effective_price(),
        ) {
            Some(resting) => {
                // a market order never rests: cancel any unfilled remainder
                if order.order_type == OrderType::Market && resting.remaining > Decimal::ZERO {
                    resting.state = OrderState::Cancelled;
                }
                if resting.state == OrderState::Cancelled {
                    let removed = self
                        .remove(
                            &order.session_id,
                            order.id,
                            order.side,
                            order.effective_price(),
                        )
                        .expect("order was just located in the book");
                    final_state = Some(removed);
                } else {
                    final_state = Some(resting.clone());
                }
            }
            None => {
                // fully consumed by matching: recover the post-fill snapshot
                for t in &trades {
                    if t.buyer.session_id == order.session_id && t.buyer.id == order.id {
                        final_state = Some(t.buyer.clone());
                    }
                    if t.seller.session_id == order.session_id && t.seller.id == order.id {
                        final_state = Some(t.seller.clone());
                    }
                }
            }
        }
        (trades, final_state)
    }

    fn insert(&mut self, order: &Order) {
        let price = order.effective_price();
        let levels = match order.side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        levels
            .entry(price)
            .or_insert_with(|| PriceLevel::new(price))
            .push_back(order.clone());
    }

    /// continuous matching: while best bid crosses best ask, trade at the
    /// resting (earlier arrival) order's price. All trades in one batch share
    /// a trade_id assigned by the engine. Mirrors matchTrades.
    fn match_trades(&mut self) -> Vec<RawTrade> {
        let mut trades = Vec::new();
        let when = SystemTime::now();
        loop {
            // snapshot the two tops
            let (bid, ask) = {
                let (Some(b), Some(a)) = (self.bids.last_key_value(), self.asks.first_key_value())
                else {
                    break;
                };
                (b.1.top().clone(), a.1.top().clone())
            };

            if bid.effective_price() < ask.effective_price() {
                break;
            }

            // trade at the resting (earlier arrival) order's price
            let price = if bid.arrival < ask.arrival {
                bid.price
            } else {
                ask.price
            };
            let quantity = min_decimal(bid.remaining, ask.remaining);

            let mut buyer = bid.clone();
            let mut seller = ask.clone();
            apply_fill(&mut buyer, quantity, price);
            apply_fill(&mut seller, quantity, price);

            for (levels, order) in [(&mut self.bids, &buyer), (&mut self.asks, &seller)] {
                let key = order.effective_price();
                let level = levels.get_mut(&key).unwrap();
                level.update_fill(order, quantity);
                if level.orders.is_empty() {
                    levels.remove(&key);
                }
            }

            trades.push(RawTrade {
                buyer,
                seller,
                price,
                quantity,
                trade_id: 0,
                when,
            });
        }
        trades
    }

    fn locate_mut(
        &mut self,
        session: &str,
        id: OrderId,
        side: Side,
        price: Decimal,
    ) -> Option<&mut Order> {
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        levels.get_mut(&price)?.locate_mut(session, id)
    }

    /// remove an order from the book; an active order becomes Cancelled.
    /// Mirrors orderBook.remove.
    pub fn remove(
        &mut self,
        session_id: &str,
        id: OrderId,
        side: Side,
        price: Decimal,
    ) -> Result<Order, ()> {
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        let level = levels.get_mut(&price).ok_or(())?;
        let removed = level.remove(session_id, id)?;
        if level.orders.is_empty() {
            levels.remove(&price);
        }
        let mut removed = removed;
        if removed.state.is_active() {
            removed.state = OrderState::Cancelled;
        }
        Ok(removed)
    }

    /// aggregate remaining quantity per price level
    pub fn build_book(&self) -> Book {
        Book {
            instrument_id: self.instrument_id,
            sequence: 0,
            bids: build_levels(self.bids.values().rev()),
            asks: build_levels(self.asks.values()),
        }
    }
}

fn build_levels<'a>(levels: impl Iterator<Item = &'a PriceLevel>) -> Vec<BookLevel> {
    levels
        .map(|level| BookLevel {
            price: level.price,
            quantity: level.total,
        })
        .collect()
}

/// Update the execution accounting carried by an order snapshot.  The engine
/// rejects non-positive order quantities at its public boundary, so a normal
/// match always has a strictly positive quantity here.  Keeping the weighted
/// average in the order itself avoids losing the history when a partially
/// filled order is cancel/replaced.
fn apply_fill(order: &mut Order, quantity: Decimal, price: Decimal) {
    let previous_cum = order.cum_quantity;
    let new_cum = previous_cum + quantity;
    if new_cum.is_zero() {
        order.avg_price = Decimal::ZERO;
    } else {
        order.avg_price = (order.avg_price * previous_cum + price * quantity) / new_cum;
    }
    order.cum_quantity = new_cum;
    order.remaining -= quantity;
    order.state = if order.remaining.is_zero() {
        OrderState::Filled
    } else {
        OrderState::PartialFill
    };
}

#[cfg(test)]
mod tests {
    use super::super::instrument::Instrument;
    use super::*;

    fn limit(
        session: &str,
        id: OrderId,
        inst: &Instrument,
        side: Side,
        price: &str,
        qty: &str,
        arrival: u64,
    ) -> Order {
        Order::limit(
            session,
            id,
            inst.id,
            side,
            price.parse().unwrap(),
            qty.parse().unwrap(),
            arrival,
        )
    }

    #[test]
    fn aggregate_and_index_stay_consistent_through_mixed_operations() {
        let mut book = OrderBook::new(1);
        for id in 1..=1000 {
            let side = if id % 3 == 0 { Side::Sell } else { Side::Buy };
            book.add(Order::limit(
                "mixed",
                id,
                1,
                side,
                Decimal::from(95 + id % 11),
                Decimal::from(1 + id % 5),
                id as u64,
            ));
            if id % 4 == 0 {
                let previous = id - 2;
                let previous_side = if previous % 3 == 0 {
                    Side::Sell
                } else {
                    Side::Buy
                };
                let _ = book.remove(
                    "mixed",
                    previous,
                    previous_side,
                    Decimal::from(95 + previous % 11),
                );
            }
            for level in book.bids.values().chain(book.asks.values()) {
                assert_eq!(
                    level.total,
                    level.orders.values().map(|o| o.remaining).sum::<Decimal>()
                );
                assert_eq!(level.index.len(), level.orders.len());
                assert!(!level.orders.is_empty());
                for (key, order) in &level.orders {
                    assert_eq!(
                        level.index.get(&(order.session_id.clone(), order.id)),
                        Some(key)
                    );
                }
            }
        }
    }

    // --- order list tests ---

    #[test]
    fn test_order_list_push_back() {
        let inst = Instrument::new(1, "AAPL");
        let mut level = PriceLevel::new("100".parse().unwrap());
        let o1 = limit("c1", 1, &inst, Side::Buy, "100", "10", 1);
        let o2 = limit("c1", 2, &inst, Side::Buy, "100", "20", 2);

        level.push_back(o1.clone());
        assert_eq!(level.orders.len(), 1);
        assert_eq!(level.top().id, o1.id);

        level.push_back(o2.clone());
        assert_eq!(level.orders.len(), 2);
        assert_eq!(level.top().id, o1.id); // o2 is at the back
    }

    #[test]
    fn test_order_list_push_front() {
        let inst = Instrument::new(1, "AAPL");
        let mut level = PriceLevel::new("100".parse().unwrap());
        let o1 = limit("c1", 1, &inst, Side::Buy, "100", "10", 1);
        let o2 = limit("c1", 2, &inst, Side::Buy, "100", "20", 2);

        level.push_back(o1.clone());
        level.push_front(o2.clone()); // goes to the front

        assert_eq!(level.orders.len(), 2);
        assert_eq!(level.top().id, o2.id);
    }

    #[test]
    fn test_order_list_remove() {
        let inst = Instrument::new(1, "AAPL");
        let mut level = PriceLevel::new("100".parse().unwrap());
        let o1 = limit("c1", 1, &inst, Side::Buy, "100", "10", 1);
        let o2 = limit("c1", 2, &inst, Side::Buy, "100", "20", 2);
        let o3 = limit("c1", 3, &inst, Side::Buy, "100", "30", 3);

        // removing from empty
        assert!(level.remove("c1", 1).is_err());

        level.push_back(o1.clone());
        level.push_back(o2.clone());
        level.push_back(o3.clone());

        // remove middle
        assert!(level.remove("c1", 2).is_ok());
        assert_eq!(level.orders.len(), 2);

        // remove head
        assert!(level.remove("c1", 1).is_ok());
        assert_eq!(level.orders.len(), 1);
        assert_eq!(level.top().id, o3.id);

        // remove tail (also head)
        assert!(level.remove("c1", 3).is_ok());
        assert_eq!(level.orders.len(), 0);
    }

    // --- order book tests ---

    #[test]
    fn test_order_book() {
        let inst = Instrument::new(0, "TEST");
        let mut ob = OrderBook::new(inst.id);

        let o1 = limit("X", 1, &inst, Side::Buy, "100", "10", 1);
        let o2 = limit("X", 2, &inst, Side::Sell, "110", "10", 2);

        let _ = ob.add(o1);
        let _ = ob.add(o2);

        let b = ob.build_book();
        assert_eq!(b.bids.len(), 1, "incorrect bids");
        assert_eq!(b.asks.len(), 1, "incorrect asks");

        let o3 = limit("X", 3, &inst, Side::Buy, "100", "10", 3);
        let o4 = limit("X", 4, &inst, Side::Buy, "99", "30", 4);

        let _ = ob.add(o3);
        let b = ob.build_book();
        assert_eq!(b.bids.len(), 1, "incorrect bids");

        let _ = ob.add(o4);
        let b = ob.build_book();
        assert_eq!(b.bids.len(), 2, "incorrect bids");
        assert_eq!(b.asks.len(), 1, "incorrect asks");
        assert_eq!(b.bids[0].quantity, "20".parse().unwrap(), "wrong quantity");

        assert!(ob.remove("X", 4, Side::Buy, "99".parse().unwrap()).is_ok());
        let b = ob.build_book();
        assert_eq!(b.bids.len(), 1, "incorrect bids");
        assert_eq!(b.asks.len(), 1, "incorrect asks");
        assert_eq!(b.bids[0].quantity, "20".parse().unwrap(), "wrong quantity");

        assert!(ob.remove("X", 3, Side::Buy, "100".parse().unwrap()).is_ok());
        let b = ob.build_book();
        assert_eq!(b.bids.len(), 1, "incorrect bids");
        assert_eq!(b.asks.len(), 1, "incorrect asks");
        assert_eq!(b.bids[0].quantity, "10".parse().unwrap(), "wrong quantity");
    }

    #[test]
    fn test_order_match() {
        let inst = Instrument::new(0, "TEST");
        let mut ob = OrderBook::new(inst.id);

        let o1 = limit("X", 1, &inst, Side::Buy, "110", "20", 1);
        let o2 = limit("X", 2, &inst, Side::Sell, "100", "10", 2);

        let _ = ob.add(o1);
        let (trades, _) = ob.add(o2);

        let b = ob.build_book();
        assert_eq!(b.bids.len(), 1, "incorrect bids");
        assert_eq!(b.asks.len(), 0, "incorrect asks");
        assert_eq!(trades.len(), 1, "wrong trades");
        assert_eq!(trades[0].quantity, "10".parse().unwrap(), "wrong trade qty");
    }

    #[test]
    fn test_order_match_sweep() {
        let inst = Instrument::new(0, "TEST");
        let mut ob = OrderBook::new(inst.id);

        let o1 = limit("X", 1, &inst, Side::Buy, "100", "20", 1);
        let o2 = limit("X", 2, &inst, Side::Buy, "90", "20", 2);
        let o3 = limit("X", 3, &inst, Side::Sell, "80", "30", 3);

        let _ = ob.add(o1);
        let _ = ob.add(o2);
        let (trades, _) = ob.add(o3);

        let b = ob.build_book();
        assert_eq!(b.bids.len(), 1, "incorrect bids");
        assert_eq!(b.asks.len(), 0, "incorrect asks");
        assert_eq!(trades.len(), 2, "wrong trades");
        assert_eq!(trades[0].quantity, "20".parse().unwrap(), "wrong trade qty");
        assert_eq!(trades[1].quantity, "10".parse().unwrap(), "wrong trade qty");
    }

    #[test]
    fn test_market_order_cancel_remainder() {
        let inst = Instrument::new(0, "TEST");
        let mut ob = OrderBook::new(inst.id);
        let _ = ob.add(limit("X", 1, &inst, Side::Buy, "100", "10", 1));
        let (trades, final_state) = ob.add(Order::market(
            "X",
            2,
            inst.id,
            Side::Sell,
            "25".parse().unwrap(),
            2,
        ));
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].quantity, "10".parse().unwrap());
        // nothing rests: the 15 share remainder is cancelled
        let b = ob.build_book();
        assert_eq!(b.bids.len(), 0);
        assert_eq!(b.asks.len(), 0);
        // the fill snapshot shows the fill-time state; the cancelled remainder
        // shows up in the final state of the order
        assert_eq!(trades[0].seller.state, OrderState::PartialFill);
        assert_eq!(final_state.as_ref().unwrap().state, OrderState::Cancelled);
        assert_eq!(
            final_state.as_ref().unwrap().remaining,
            "15".parse().unwrap()
        );
    }

    #[test]
    fn test_trade_price_is_resting_order_price() {
        let inst = Instrument::new(0, "TEST");
        let mut ob = OrderBook::new(inst.id);
        let _ = ob.add(limit("a", 1, &inst, Side::Buy, "101", "5", 1));
        let (trades, _) = ob.add(limit("b", 1, &inst, Side::Sell, "99", "5", 2));
        assert_eq!(trades.len(), 1);
        // buyer rested first, so the trade happens at the buyer's price
        assert_eq!(trades[0].price, "101".parse().unwrap());
    }
}
