//! Limits parser input before decoding, and encoded proof size before retention.
use super::*;
use serde::de::{DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use std::{
    cell::Cell,
    fmt,
    io::{self, Read, Write},
};

pub(super) const BUFFER: usize = 64 * 1024;
pub(super) const RECORD: usize = 64 * 1024 * 1024;
pub(super) const SOURCE: usize = 16 * 1024;
pub(super) const RAW_RECORD: usize = 16 * 1024 * 1024;
pub(super) const COUNT: usize = 65_536;
// One read can hash a prefix, decode its final sidecar, validate raw evidence,
// and recover selected originals. Charge all these passes across all owners.
pub(super) const READ_WORK: u64 = 256 * 1024 * 1024;
#[derive(Clone)]
pub(in crate::desktop_store::memory) struct ReadWork(std::rc::Rc<Cell<u64>>);
impl Default for ReadWork {
    fn default() -> Self {
        Self(std::rc::Rc::new(Cell::new(READ_WORK)))
    }
}
impl ReadWork {
    pub(super) fn charge(&self, bytes: u64) -> StoreResult<()> {
        let remaining = self
            .0
            .get()
            .checked_sub(bytes)
            .ok_or(StoreError::Overflow)?;
        self.0.set(remaining);
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct Budget {
    bytes: Cell<u64>,
    end: Cell<u64>,
}
impl Budget {
    pub(super) fn arm(&self, limit: usize) -> StoreResult<()> {
        self.end.set(
            self.bytes
                .get()
                .checked_add(limit as u64)
                .ok_or(StoreError::Overflow)?,
        );
        Ok(())
    }
}
/// This wrapper must be OUTSIDE BufReader. Its fixed prefetch allocation is
/// separate from the bytes supplied to serde's single-byte IoRead cursor.
pub(super) struct Limited<'a, R> {
    pub(super) inner: R,
    pub(super) budget: &'a Budget,
}
impl<R: Read> Read for Limited<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let remaining = self
            .budget
            .end
            .get()
            .saturating_sub(self.budget.bytes.get());
        if remaining == 0 {
            return Err(io::Error::other("Pinned memory parser input limit"));
        }
        let limit = out
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let count = self.inner.read(&mut out[..limit])?;
        self.budget.bytes.set(
            self.budget
                .bytes
                .get()
                .checked_add(count as u64)
                .ok_or_else(|| io::Error::other("Pinned memory input position limit"))?,
        );
        Ok(count)
    }
}

struct Counter {
    bytes: usize,
    limit: usize,
}
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .filter(|n| *n <= self.limit)
            .ok_or_else(|| io::Error::other("Pinned memory retention limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub(super) fn encoded_size(value: &impl Serialize, limit: usize) -> StoreResult<usize> {
    let mut count = Counter { bytes: 0, limit };
    serde_json::to_writer(&mut count, value)?;
    Ok(count.bytes)
}

pub(super) fn key<'de, A: MapAccess<'de>>(
    map: &mut A,
    budget: &Budget,
) -> Result<Option<String>, A::Error> {
    budget.arm(256).map_err(serde::de::Error::custom)?;
    map.next_key()
}
pub(super) fn value<'de, A, T>(map: &mut A, budget: &Budget, limit: usize) -> Result<T, A::Error>
where
    A: MapAccess<'de>,
    T: DeserializeOwned + Serialize,
{
    budget
        .arm(limit.saturating_add(2))
        .map_err(serde::de::Error::custom)?;
    let item = map.next_value()?;
    encoded_size(&item, limit).map_err(serde::de::Error::custom)?;
    Ok(item)
}
pub(super) fn sequence<'de, A, T>(
    map: &mut A,
    budget: &Budget,
    limit: usize,
    max: usize,
    mut accept: impl FnMut(T) -> StoreResult<()>,
) -> Result<(), A::Error>
where
    A: MapAccess<'de>,
    T: DeserializeOwned + Serialize,
{
    budget.arm(256).map_err(serde::de::Error::custom)?;
    map.next_value_seed(Items {
        budget,
        limit,
        max,
        accept: &mut accept,
        marker: std::marker::PhantomData,
    })
}
struct Items<'a, T, F> {
    budget: &'a Budget,
    limit: usize,
    max: usize,
    accept: &'a mut F,
    marker: std::marker::PhantomData<T>,
}
impl<'de, T: DeserializeOwned + Serialize, F: FnMut(T) -> StoreResult<()>> DeserializeSeed<'de>
    for Items<'_, T, F>
{
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_seq(self)
    }
}
struct RejectExtra;
impl<'de> DeserializeSeed<'de> for RejectExtra {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, _: D) -> Result<(), D::Error> {
        Err(serde::de::Error::custom("Pinned memory array count limit"))
    }
}
impl<'de, T: DeserializeOwned + Serialize, F: FnMut(T) -> StoreResult<()>> Visitor<'de>
    for Items<'_, T, F>
{
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("bounded saved records")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        for _ in 0..self.max {
            self.budget
                .arm(self.limit.saturating_add(2))
                .map_err(serde::de::Error::custom)?;
            let Some(item) = seq.next_element::<T>()? else {
                return Ok(());
            };
            encoded_size(&item, self.limit).map_err(serde::de::Error::custom)?;
            (self.accept)(item).map_err(serde::de::Error::custom)?;
        }
        self.budget.arm(256).map_err(serde::de::Error::custom)?;
        seq.next_element_seed(RejectExtra)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Envelope<'a> {
        budget: &'a Budget,
        limit: usize,
        count: usize,
    }
    impl<'de> DeserializeSeed<'de> for Envelope<'_> {
        type Value = Vec<String>;
        fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            d.deserialize_map(self)
        }
    }
    impl<'de> Visitor<'de> for Envelope<'_> {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("test records")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut values = None;
            while let Some(name) = key(&mut map, self.budget)? {
                if name != "items" || values.is_some() {
                    return Err(serde::de::Error::custom("invalid field"));
                }
                let mut out = Vec::new();
                sequence(
                    &mut map,
                    self.budget,
                    self.limit,
                    self.count,
                    |value: String| {
                        out.push(value);
                        Ok(())
                    },
                )?;
                values = Some(out);
            }
            values.ok_or_else(|| serde::de::Error::custom("missing items"))
        }
    }
    fn parse(
        input: &[u8],
        limit: usize,
        count: usize,
        buffer: usize,
    ) -> Result<Vec<String>, serde_json::Error> {
        let budget = Budget::default();
        budget.arm(256).unwrap();
        let reader = Limited {
            inner: BufReader::with_capacity(buffer, Cursor::new(input)),
            budget: &budget,
        };
        let mut parser = serde_json::Deserializer::from_reader(reader);
        let output = Envelope {
            budget: &budget,
            limit,
            count,
        }
        .deserialize(&mut parser)?;
        budget.arm(256).unwrap();
        parser.end()?;
        Ok(output)
    }
    #[test]
    fn frozen_feed_and_retained_record_bounds_reject_oversize_without_reading_the_tail() {
        for buffer in [1, 7, BUFFER] {
            let exact = "x".repeat(SOURCE - 2);
            let input = serde_json::to_vec(&serde_json::json!({"items":[&exact]})).unwrap();
            assert_eq!(parse(&input, SOURCE, 1, buffer).unwrap(), vec![exact]);
            let over =
                serde_json::to_vec(&serde_json::json!({"items":["x".repeat(SOURCE-1)]})).unwrap();
            assert!(parse(&over, SOURCE, 1, buffer).is_err());
            let huge =
                serde_json::to_vec(&serde_json::json!({"items":["x".repeat(SOURCE*4)]})).unwrap();
            let error = parse(&huge, SOURCE, 1, buffer).unwrap_err();
            assert!(error.to_string().contains("parser input limit"), "{error}");
        }
    }
    #[test]
    fn frozen_map_count_key_whitespace_and_escaped_content_are_bounded() {
        assert_eq!(
            parse(br#"{"it\u0065ms":["a\"b", "two"]}"#, SOURCE, 2, BUFFER).unwrap(),
            vec!["a\"b", "two"]
        );
        assert!(parse(br#"{"items":[],"items":[]}"#, SOURCE, 2, BUFFER).is_err());
        assert!(parse(br#"{"unknown":[],"items":[]}"#, SOURCE, 2, BUFFER).is_err());
        let huge_extra = format!("{{\"items\":[\"one\",\"{}\"]}}", "x".repeat(SOURCE * 4));
        assert!(
            parse(huge_extra.as_bytes(), SOURCE, 1, BUFFER)
                .unwrap_err()
                .to_string()
                .contains("array count limit")
        );
        let whitespace = format!("{{\"items\":[{}\"x\"]}}", " ".repeat(SOURCE * 4));
        assert!(
            parse(whitespace.as_bytes(), SOURCE, 1, BUFFER)
                .unwrap_err()
                .to_string()
                .contains("parser input limit")
        );
        let key = format!("{{\"{}\":[]}}", "x".repeat(SOURCE));
        assert!(
            parse(key.as_bytes(), SOURCE, 1, BUFFER)
                .unwrap_err()
                .to_string()
                .contains("parser input limit")
        );
    }
    #[test]
    fn frozen_read_work_is_shared_across_scans_and_inclusive() {
        let work = ReadWork::default();
        let parent = work.clone();
        let selected_raw = work.clone();
        work.charge(READ_WORK / 2).unwrap();
        parent.charge(READ_WORK / 2 - 1).unwrap();
        selected_raw.charge(1).unwrap();
        assert!(matches!(work.charge(1), Err(StoreError::Overflow)));
        assert!(matches!(parent.charge(1), Err(StoreError::Overflow)));
        assert_eq!(selected_raw.0.get(), 0);
    }

    #[test]
    fn frozen_compact_proof_actual_64_mib_limit_is_inclusive() {
        let mut value = "x".repeat(RECORD - 2);
        assert_eq!(encoded_size(&value, RECORD).unwrap(), RECORD);
        value.push('x');
        assert!(encoded_size(&value, RECORD).is_err());
    }
}
