use std::collections::HashMap;
use std::io::BufRead;

/// a tradable instrument
#[derive(Clone, Debug, PartialEq)]
pub struct Instrument {
    pub id: i64,
    pub symbol: String,
}

impl Instrument {
    pub fn new(id: i64, symbol: &str) -> Instrument {
        Instrument {
            id,
            symbol: symbol.to_string(),
        }
    }
}

/// instrument registry; bidirectional symbol/id lookup
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

    /// sorted for deterministic output
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
        self.by_symbol
            .insert(instrument.symbol.clone(), instrument.clone());
        self.by_id.insert(instrument.id, instrument);
    }

    /// load "INSTRUMENT_ID SYMBOL" lines from a reader; '#' or '//' lines are
    /// comments.
    pub fn load_from_reader<R: BufRead>(&mut self, reader: R) -> std::io::Result<()> {
        // Parse into a separate registry: any failure leaves the live map intact.
        let mut loaded = InstrumentMap::new();
        for (index, line) in reader.lines().enumerate() {
            let s = line?;
            let s = s.trim();
            if s.is_empty() || s.starts_with("//") || s.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = s.split_whitespace().collect();
            let invalid = |message: &str| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("instruments line {}: {message}", index + 1),
                )
            };
            if parts.len() != 2 {
                return Err(invalid("expected exactly INSTRUMENT_ID SYMBOL"));
            }
            let id = parts[0]
                .parse::<i64>()
                .map_err(|_| invalid("ID must be a positive integer"))?;
            if id <= 0 || id == i64::MAX {
                return Err(invalid("ID must be between 1 and 9223372036854775806"));
            }
            if !parts[1].bytes().all(|b| b.is_ascii_graphic() && b != b'=') {
                return Err(invalid(
                    "SYMBOL must be printable ASCII without '=' or whitespace",
                ));
            }
            if loaded.get_by_id(id).is_some() {
                return Err(invalid("duplicate INSTRUMENT_ID"));
            }
            if loaded.get_by_symbol(parts[1]).is_some() {
                return Err(invalid("duplicate SYMBOL"));
            }
            loaded.next_id = loaded.next_id.max(id);
            loaded.put(Instrument::new(id, parts[1]));
        }
        if loaded.by_id.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "instruments file must contain at least one instrument",
            ));
        }
        *self = loaded;
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
        let data = "# comment\n1 IBM\n2 AAPL\n\n// c++ style\n3 NFLX\n";
        let mut map = InstrumentMap::new();
        map.load_from_reader(data.as_bytes()).unwrap();
        assert!(map.get_by_symbol("IBM").is_some());
        assert_eq!(map.get_by_symbol("IBM").unwrap().id, 1);
        assert_eq!(map.get_by_id(2).unwrap().symbol, "AAPL");
        assert_eq!(map.all_symbols(), vec!["AAPL", "IBM", "NFLX"]);
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

    #[test]
    fn invalid_load_is_atomic() {
        for data in [
            "",
            "# empty\n",
            "bad IBM\n",
            "0 IBM\n",
            "-1 IBM\n",
            "1 IBM extra\n",
            "1 IBM\n1 AAPL\n",
            "1 IBM\n2 IBM\n",
            "1 I=BM\n",
            "9223372036854775807 IBM\n",
        ] {
            let mut map = InstrumentMap::new();
            map.load_from_reader("7 ORIGINAL\n".as_bytes()).unwrap();
            assert!(map.load_from_reader(data.as_bytes()).is_err(), "{data:?}");
            assert_eq!(map.all_symbols(), vec!["ORIGINAL"]);
            assert_eq!(map.next_id(), 8);
        }
    }
}
