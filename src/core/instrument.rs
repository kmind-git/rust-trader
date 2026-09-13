use std::collections::HashMap;
use std::io::BufRead;

/// a tradable instrument; mirrors common.Instrument
#[derive(Clone, Debug, PartialEq)]
pub struct Instrument {
    pub id: i64,
    pub symbol: String,
}

impl Instrument {
    pub fn new(id: i64, symbol: &str) -> Instrument {
        Instrument { id, symbol: symbol.to_string() }
    }
}

/// instrument registry; mirrors common.IMap (instrumentmap.go)
#[derive(Default)]
pub struct InstrumentMap {
    by_symbol: HashMap<String, Instrument>,
    by_id: HashMap<i64, Instrument>,
    next_id: i64,
}

impl InstrumentMap {
    pub fn new() -> InstrumentMap {
        InstrumentMap::default()
    }

    pub fn get_by_symbol(&self, symbol: &str) -> Option<&Instrument> {
        self.by_symbol.get(symbol)
    }

    pub fn get_by_id(&self, id: i64) -> Option<&Instrument> {
        self.by_id.get(&id)
    }

    /// sorted for deterministic output (Go map iteration order is random)
    pub fn all_symbols(&self) -> Vec<String> {
        let mut symbols: Vec<String> = self.by_symbol.keys().cloned().collect();
        symbols.sort();
        symbols
    }

    /// allocate an id for a dynamically created instrument
    pub fn next_id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }

    pub fn put(&mut self, instrument: Instrument) {
        self.by_symbol.insert(instrument.symbol.clone(), instrument.clone());
        self.by_id.insert(instrument.id, instrument);
    }

    /// load "INSTRUMENT_ID SYMBOL" lines from a reader; '#' or '//' lines are
    /// comments. mirrors IMap.Load in Go.
    pub fn load_from_reader<R: BufRead>(&mut self, reader: R) -> std::io::Result<()> {
        for line in reader.lines() {
            let s = line?;
            let s = s.trim();
            if s.is_empty() || s.starts_with("//") || s.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = s.split_whitespace().collect();
            if parts.len() == 2 {
                let id: i64 = parts[0].parse().unwrap_or(0);
                let instrument = Instrument::new(id, parts[1]);
                if id > self.next_id {
                    // ensure the next dynamic instrument does not collide with loaded ones
                    self.next_id = id;
                }
                self.put(instrument);
            }
        }
        Ok(())
    }

    pub fn load(&mut self, path: &str) -> std::io::Result<()> {
        let file = std::fs::File::open(path)?;
        self.load_from_reader(std::io::BufReader::new(file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_and_next_id() {
        let data = "# comment\n1 IBM\n2 AAPL\n\n// c++ style\n3 NFLX\nbad line\n";
        let mut map = InstrumentMap::new();
        map.load_from_reader(data.as_bytes()).unwrap();
        assert!(map.get_by_symbol("IBM").is_some());
        assert_eq!(map.get_by_symbol("IBM").unwrap().id, 1);
        assert_eq!(map.get_by_id(2).unwrap().symbol, "AAPL");
        assert_eq!(map.all_symbols(), vec!["AAPL", "IBM", "NFLX", "line"]);
        // next dynamic id must not collide with loaded ids
        assert_eq!(map.next_id(), 4);
    }

    #[test]
    fn test_dynamic_instrument() {
        let mut map = InstrumentMap::new();
        let id = map.next_id();
        map.put(Instrument::new(id, "TSLA"));
        assert!(map.get_by_symbol("TSLA").is_some());
        assert_eq!(map.get_by_id(1).unwrap().symbol, "TSLA");
    }
}
