//! Latest complete market state, shared independently of the trading lock.
//!
//! Each instrument has one atomic snapshot slot. Readers may skip versions;
//! reading never consumes a snapshot. An owned Arc keeps an older version safe
//! while a publisher replaces the slot. This is not a trade-report transport.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::SystemTime;

use arc_swap::ArcSwap;

use crate::core::instrument::Instrument;
use crate::core::orderbook::Book;
use crate::core::stats::Statistics;

/// Book, cumulative statistics and version from a single publication.
/// Version zero represents a known instrument without market activity.
#[derive(Debug)]
pub struct InstrumentSnapshot {
    pub book: Book,
    pub stats: Statistics,
    pub version: u64,
    pub published_at: SystemTime,
}

type Slot = Arc<ArcSwap<InstrumentSnapshot>>;
type Catalog = BTreeMap<String, Slot>;

/// Cloneable read access; no Engine reference or trading mutex is needed.
#[derive(Clone)]
pub struct MarketDataReader {
    catalog: Arc<ArcSwap<Catalog>>,
}

impl MarketDataReader {
    /// Load one complete version. Concurrent updates can be observed by the
    /// next call; they cannot alter the version returned by this call.
    pub fn snapshot(&self, symbol: &str) -> Option<Arc<InstrumentSnapshot>> {
        let catalog = self.catalog.load();
        catalog.get(symbol).map(|slot| slot.load_full())
    }

    pub fn all_symbols(&self) -> Vec<String> {
        self.catalog.load().keys().cloned().collect()
    }
}

/// The Engine owns the only publisher and serializes its updates. Keeping this
/// separate from the cloneable reader prevents accidental competing publishers.
pub(crate) struct MarketDataPublisher {
    reader: MarketDataReader,
    by_id: HashMap<i64, Slot>,
}

impl MarketDataPublisher {
    pub(crate) fn new() -> Self {
        Self {
            reader: MarketDataReader {
                catalog: Arc::new(ArcSwap::from_pointee(Catalog::new())),
            },
            by_id: HashMap::new(),
        }
    }

    pub(crate) fn reader(&self) -> MarketDataReader {
        self.reader.clone()
    }

    /// Catalog copying happens only on instrument registration, never per tick.
    /// Existing slots survive registration, so existing readers remain live.
    pub(crate) fn register<'a>(&mut self, instruments: impl Iterator<Item = &'a Instrument>) {
        let mut catalog = (**self.reader.catalog.load()).clone();
        for instrument in instruments {
            let slot = self.by_id.entry(instrument.id).or_insert_with(|| {
                Arc::new(ArcSwap::from_pointee(InstrumentSnapshot {
                    book: Book {
                        instrument_id: instrument.id,
                        sequence: 0,
                        bids: Vec::new(),
                        asks: Vec::new(),
                    },
                    stats: Statistics {
                        symbol: instrument.symbol.clone(),
                        ..Statistics::default()
                    },
                    version: 0,
                    published_at: SystemTime::now(),
                }))
            });
            catalog.insert(instrument.symbol.clone(), Arc::clone(slot));
        }
        self.reader.catalog.store(Arc::new(catalog));
    }

    pub(crate) fn snapshot(&self, instrument_id: i64) -> Option<Arc<InstrumentSnapshot>> {
        self.by_id.get(&instrument_id).map(|slot| slot.load_full())
    }

    pub(crate) fn publish(&mut self, book: Book, stats: Statistics) {
        let slot = self
            .by_id
            .get(&book.instrument_id)
            .expect("market data instrument must be registered before trading");
        let version = book.sequence;
        slot.store(Arc::new(InstrumentSnapshot {
            book,
            stats,
            version,
            published_at: SystemTime::now(),
        }));
    }
}
