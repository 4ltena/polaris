//! Run-owned catalog accounting, independent of connection and future lifetimes.
use super::{Error, MAX_RUN_CATALOG_BYTES, Value, bounded_json};

/// A run keeps this value through refresh, retry, eviction and cancellation.
/// Per-catalog limits are separately enforced by Builder for each acquisition.
#[derive(Default)]
pub struct Budget {
    received: usize,
    exhausted: bool,
}

impl Budget {
    pub fn bytes_received(&self) -> usize {
        self.received
    }

    pub fn check_available(&self) -> Result<(), Error> {
        if self.exhausted || self.received >= MAX_RUN_CATALOG_BYTES {
            Err(Error::Limit)
        } else {
            Ok(())
        }
    }

    /// Called synchronously after decoding, before page/item validation or
    /// another await. Rejected and subsequently abandoned pages still count.
    pub(crate) fn receive_page(&mut self, items: &[Value]) -> Result<(), Error> {
        self.check_available()?;
        for item in items {
            let bytes = match bounded_json(item, MAX_RUN_CATALOG_BYTES) {
                Ok(bytes) => bytes.len(),
                Err(error) => {
                    // The exact length is unknown; do not reopen capacity.
                    self.exhausted = true;
                    return Err(error);
                }
            };
            self.received = self.received.saturating_add(bytes);
            if self.received > MAX_RUN_CATALOG_BYTES {
                self.exhausted = true;
                return Err(Error::Limit);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Builder, Kind};
    use serde_json::json;

    #[test]
    fn rejected_items_and_bad_cursor_are_charged_before_builder_validation() {
        for next in [None, Some(String::new())] {
            let items = vec![json!({"unexpected":"malformed"})];
            let cost = serde_json::to_vec(&items[0]).unwrap().len();
            let mut budget = Budget::default();
            let mut builder = Builder::new("server", &json!({}), [Kind::Tool]).unwrap();
            budget.receive_page(&items).unwrap();
            let result = builder.add_page(Kind::Tool, None, items, next.clone());
            if next.is_some() {
                assert!(result.is_err());
            } else {
                result.unwrap();
                let catalog = builder.finish().unwrap();
                assert_eq!(catalog.rejected_items, 1);
                assert_eq!(catalog.encoded_bytes, cost);
            }
            assert_eq!(budget.bytes_received(), cost);
        }
    }

    #[test]
    fn exact_run_limit_is_allowed_but_no_following_request_is_admitted() {
        let mut budget = Budget::default();
        let item = Value::String("x".repeat(MAX_RUN_CATALOG_BYTES - 2));
        budget.receive_page(&[item]).unwrap();
        assert_eq!(budget.bytes_received(), MAX_RUN_CATALOG_BYTES);
        assert_eq!(budget.check_available(), Err(Error::Limit));
        assert_eq!(budget.receive_page(&[json!({})]), Err(Error::Limit));

        let mut budget = Budget::default();
        let item = Value::String("x".repeat(MAX_RUN_CATALOG_BYTES - 1));
        assert_eq!(budget.receive_page(&[item]), Err(Error::Limit));
        assert_eq!(budget.check_available(), Err(Error::Limit));
    }
}
