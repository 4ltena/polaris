//! Bounded, SDK-independent catalog data. Descriptions and annotations are
//! untrusted data; neither aliases nor catalog presence authorize an operation.

mod budget;
pub mod index;
pub use budget::Budget;

use crate::{
    Error,
    registration::{bounded_json, canonical_hash, valid_id},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_SERVER_CATALOG_BYTES: usize = 1024 * 1024;
pub const MAX_RUN_CATALOG_BYTES: usize = 4 * 1024 * 1024;
const MAX_ITEMS: usize = 2048;
const MAX_PAGES: usize = 64;
const MAX_METADATA_BYTES: usize = 16 * 1024;
const MAX_CURSOR_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Tool,
    Resource,
    ResourceTemplate,
    Prompt,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ItemId {
    pub server_id: String,
    pub kind: Kind,
    /// Tool/prompt name, resource URI, or resource URI template.
    pub key: String,
}

/// Stable provider name for a tool identity; never an authorization key.
pub fn tool_alias(id: &ItemId) -> Result<String, Error> {
    if id.kind != Kind::Tool {
        return Err(Error::Protocol);
    }
    canonical_hash(&serde_json::json!(id)).map(|hash| format!("mcp_{}", &hash[..60]))
}

#[derive(Clone, Debug, Serialize)]
pub struct Item {
    pub id: ItemId,
    pub name: String,
    pub description: String,
    pub metadata: Value,
    pub input_schema: Option<Value>,
    pub output_schema: Option<Value>,
    pub schema_hash: Option<String>,
    pub alias: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Catalog {
    pub generation: String,
    pub items: Vec<Item>,
    pub encoded_bytes: usize,
    /// Invalid items are quarantined; this catalog must be shown as partial.
    pub rejected_items: usize,
}

#[derive(Default)]
struct Pages {
    count: usize,
    items: usize,
    cursors: BTreeSet<String>,
    keys: BTreeSet<String>,
    next: Option<String>,
    complete: bool,
}

/// A host creates a fresh builder for every refresh. Failure never publishes
/// the builder as a complete catalog or silently preserves an older snapshot.
pub(crate) struct Builder {
    server: String,
    binding: String,
    pages: BTreeMap<Kind, Pages>,
    items: BTreeMap<ItemId, Item>,
    hashes: Vec<String>,
    bytes: usize,
    rejected: usize,
    failed: bool,
}

impl Builder {
    pub fn new(
        server: &str,
        binding: &Value,
        kinds: impl IntoIterator<Item = Kind>,
    ) -> Result<Self, Error> {
        if !valid_id(server) {
            return Err(Error::InvalidRegistration);
        }
        Ok(Self {
            server: server.into(),
            binding: canonical_hash(binding)?,
            pages: kinds
                .into_iter()
                .map(|kind| (kind, Pages::default()))
                .collect(),
            items: BTreeMap::new(),
            hashes: Vec::new(),
            bytes: 0,
            rejected: 0,
            failed: false,
        })
    }

    pub fn add_page(
        &mut self,
        kind: Kind,
        cursor: Option<&str>,
        values: Vec<Value>,
        next: Option<String>,
    ) -> Result<(), Error> {
        if self.failed {
            return Err(Error::Protocol);
        }
        let result = self.add_page_inner(kind, cursor, values, next);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn add_page_inner(
        &mut self,
        kind: Kind,
        cursor: Option<&str>,
        values: Vec<Value>,
        next: Option<String>,
    ) -> Result<(), Error> {
        let page = self.pages.get_mut(&kind).ok_or(Error::Protocol)?;
        if page.complete
            || page.next.as_deref() != cursor
            || page.count >= MAX_PAGES
            || values.len() > MAX_ITEMS - page.items
        {
            return Err(Error::Limit);
        }
        if next.as_ref().is_some_and(|next| {
            next.is_empty() || next.len() > MAX_CURSOR_BYTES || next.chars().any(char::is_control)
        }) {
            return Err(Error::Limit);
        }
        if let Some(next) = &next
            && (!page.cursors.insert(next.clone()) || values.is_empty())
        {
            return Err(Error::Protocol);
        }
        page.count += 1;
        page.items += values.len();
        if next.is_some() && (page.count == MAX_PAGES || page.items == MAX_ITEMS) {
            return Err(Error::Limit);
        }
        let mut new_keys = 0;
        for value in values {
            // Count all received items, including ones which will be quarantined.
            let bytes = bounded_json(&value, MAX_SERVER_CATALOG_BYTES)?;
            self.bytes = self.bytes.checked_add(bytes.len()).ok_or(Error::Limit)?;
            if self.bytes > MAX_SERVER_CATALOG_BYTES {
                return Err(Error::Limit);
            }
            let key_field = match kind {
                Kind::Resource => "uri",
                Kind::ResourceTemplate => "uriTemplate",
                Kind::Tool | Kind::Prompt => "name",
            };
            if let Some(key) = value.get(key_field).and_then(Value::as_str)
                && !page.keys.insert(key.to_owned())
            {
                // A quarantined definition must not let a later definition
                // reuse the same operation identity with ambiguous semantics.
                return Err(Error::Protocol);
            }
            match parse_item(&self.server, kind, value) {
                Ok((item, hash)) => {
                    new_keys += 1;
                    self.hashes.push(hash);
                    self.items.insert(item.id.clone(), item);
                }
                Err(_) => {
                    self.rejected += 1;
                    // Invalid payloads still affect the generation and cannot
                    // cause a previous approval to match a changed catalog.
                    self.hashes
                        .push(format!("rejected:{:x}", Sha256::digest(bytes)));
                }
            }
        }
        if next.is_some() && new_keys == 0 {
            return Err(Error::Protocol);
        }
        page.complete = next.is_none();
        page.next = next;
        Ok(())
    }

    pub fn finish(mut self) -> Result<Catalog, Error> {
        if self.failed || self.pages.values().any(|page| !page.complete) {
            return Err(Error::Protocol);
        }
        self.hashes.sort();
        let mut digest = Sha256::new();
        digest.update(self.binding.as_bytes());
        digest.update((self.server.len() as u64).to_be_bytes());
        digest.update(self.server.as_bytes());
        for hash in self.hashes {
            digest.update(hash.as_bytes());
        }
        Ok(Catalog {
            generation: format!("{:x}", digest.finalize()),
            items: self.items.into_values().collect(),
            encoded_bytes: self.bytes,
            rejected_items: self.rejected,
        })
    }
}

fn parse_item(server: &str, kind: Kind, value: Value) -> Result<(Item, String), Error> {
    let mut metadata = value.as_object().cloned().ok_or(Error::Protocol)?;
    let name = metadata
        .get("name")
        .and_then(Value::as_str)
        .ok_or(Error::Protocol)?
        .to_owned();
    if name.is_empty() || name.chars().any(char::is_control) {
        return Err(Error::Protocol);
    }
    if kind == Kind::Tool
        && (name.len() > 128
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)))
    {
        return Err(Error::Protocol);
    }
    let key = match kind {
        Kind::Resource => metadata
            .get("uri")
            .and_then(Value::as_str)
            .ok_or(Error::Protocol)?,
        Kind::ResourceTemplate => metadata
            .get("uriTemplate")
            .and_then(Value::as_str)
            .ok_or(Error::Protocol)?,
        _ => &name,
    }
    .to_owned();
    if key.is_empty() || key.chars().any(char::is_control) {
        return Err(Error::Protocol);
    }
    let description = match metadata.get("description") {
        None => String::new(),
        Some(Value::String(value)) => value.clone(),
        _ => return Err(Error::Protocol),
    };
    let (input_schema, output_schema) = if kind == Kind::Tool {
        (
            Some(metadata.remove("inputSchema").ok_or(Error::Protocol)?),
            metadata.remove("outputSchema"),
        )
    } else {
        (None, None)
    };
    let mut schema_hashes = Vec::new();
    for schema in input_schema.iter().chain(output_schema.iter()) {
        if !schema.is_object() {
            return Err(Error::Protocol);
        }
        schema_hashes.push(canonical_hash(schema)?);
    }
    let metadata = Value::Object(metadata);
    bounded_json(&metadata, MAX_METADATA_BYTES)?;
    let id = ItemId {
        server_id: server.into(),
        kind,
        key,
    };
    let alias = (kind == Kind::Tool).then(|| tool_alias(&id)).transpose()?;
    let schema_hash = input_schema
        .as_ref()
        .map(|_| canonical_hash(&serde_json::json!(schema_hashes)))
        .transpose()?;
    let hash = canonical_hash(
        &serde_json::json!({"id":id,"metadata":canonical_hash(&metadata)?,"schemas":schema_hashes}),
    )?;
    Ok((
        Item {
            id,
            name,
            description,
            metadata,
            input_schema,
            output_schema,
            schema_hash,
            alias,
        },
        hash,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> Value {
        json!({"name":name,"description":"untrusted fixture", "inputSchema":{"type":"object"}})
    }
    fn builder(server: &str) -> Builder {
        Builder::new(
            server,
            &json!({"registry":1,"connection":"fixture"}),
            [Kind::Tool],
        )
        .unwrap()
    }

    #[test]
    fn stable_catalog_ignores_page_and_object_order_but_binds_identity_and_schema() {
        let mut first = builder("one");
        first
            .add_page(Kind::Tool, None, vec![tool("a")], Some("next".into()))
            .unwrap();
        first
            .add_page(Kind::Tool, Some("next"), vec![tool("b")], None)
            .unwrap();
        let first = first.finish().unwrap();
        let mut reordered = builder("one");
        reordered
            .add_page(Kind::Tool, None, vec![tool("b"), tool("a")], None)
            .unwrap();
        assert_eq!(first.generation, reordered.finish().unwrap().generation);
        let mut other = builder("two");
        other
            .add_page(Kind::Tool, None, vec![tool("a"), tool("b")], None)
            .unwrap();
        let other = other.finish().unwrap();
        assert_ne!(first.generation, other.generation);
        assert_ne!(first.items[0].id, other.items[0].id);
        assert_ne!(first.items[0].alias, other.items[0].alias);
        assert_eq!(first.items[0].alias.as_ref().unwrap().len(), 64);
        let mut changed = builder("one");
        let mut item = tool("a");
        item["inputSchema"]["required"] = json!(["x"]);
        changed
            .add_page(Kind::Tool, None, vec![item, tool("b")], None)
            .unwrap();
        let changed = changed.finish().unwrap();
        assert_ne!(first.generation, changed.generation);
        assert_ne!(first.items[0].schema_hash, changed.items[0].schema_hash);
    }

    #[test]
    fn cursor_cycles_duplicates_and_nonprogress_pages_never_publish_success() {
        let mut cycle = builder("one");
        cycle
            .add_page(Kind::Tool, None, vec![tool("a")], Some("x".into()))
            .unwrap();
        assert!(
            cycle
                .add_page(Kind::Tool, Some("x"), vec![tool("b")], Some("x".into()))
                .is_err()
        );
        assert!(cycle.finish().is_err());
        for values in [vec![], vec![tool("a")]] {
            let mut no_progress = builder("one");
            no_progress
                .add_page(Kind::Tool, None, vec![tool("a")], Some("x".into()))
                .unwrap();
            assert!(
                no_progress
                    .add_page(Kind::Tool, Some("x"), values, Some("different".into()))
                    .is_err()
            );
            assert!(no_progress.finish().is_err());
        }
        let mut wrong_cursor = builder("one");
        assert!(
            wrong_cursor
                .add_page(Kind::Tool, Some("not-requested"), vec![], None)
                .is_err()
        );
        assert!(wrong_cursor.finish().is_err());
        let mut incomplete = builder("one");
        incomplete
            .add_page(Kind::Tool, None, vec![tool("a")], Some("x".into()))
            .unwrap();
        assert!(incomplete.finish().is_err());
    }

    #[test]
    fn page_item_cursor_and_server_byte_caps_stop_before_another_request() {
        let mut pages = builder("one");
        let mut cursor = None;
        for n in 0..MAX_PAGES {
            let next = Some(format!("{n}"));
            let result = pages.add_page(
                Kind::Tool,
                cursor.as_deref(),
                vec![tool(&format!("tool{n}"))],
                next.clone(),
            );
            if n == MAX_PAGES - 1 {
                assert_eq!(result, Err(Error::Limit));
            } else {
                result.unwrap();
            }
            cursor = next;
        }
        let mut items = builder("one");
        let values = (0..MAX_ITEMS).map(|n| tool(&format!("t{n}"))).collect();
        items.add_page(Kind::Tool, None, values, None).unwrap();
        assert_eq!(items.finish().unwrap().items.len(), MAX_ITEMS);
        let mut too_many = builder("one");
        assert_eq!(
            too_many.add_page(Kind::Tool, None, vec![tool("a"); MAX_ITEMS + 1], None),
            Err(Error::Limit)
        );
        let mut cursor = builder("one");
        assert_eq!(
            cursor.add_page(
                Kind::Tool,
                None,
                vec![tool("a")],
                Some("x".repeat(MAX_CURSOR_BYTES + 1))
            ),
            Err(Error::Limit)
        );
        let mut bytes = builder("one");
        let mut huge = tool("a");
        huge["description"] = json!("x".repeat(MAX_SERVER_CATALOG_BYTES));
        assert_eq!(
            bytes.add_page(Kind::Tool, None, vec![huge], None),
            Err(Error::Limit)
        );
        assert!(bytes.finish().is_err());
    }

    #[test]
    fn invalid_items_are_visible_as_partial_without_unbounded_metadata() {
        let mut catalog = builder("one");
        let mut oversize = tool("large");
        oversize["description"] = json!("x".repeat(MAX_METADATA_BYTES));
        catalog
            .add_page(
                Kind::Tool,
                None,
                vec![tool("ok"), tool("bad/name"), oversize],
                None,
            )
            .unwrap();
        let catalog = catalog.finish().unwrap();
        assert_eq!(catalog.items.len(), 1);
        assert_eq!(catalog.rejected_items, 2);
        assert!(catalog.encoded_bytes > MAX_METADATA_BYTES);
        let mut duplicate = builder("one");
        let mut invalid = tool("same");
        invalid["inputSchema"] = json!(null);
        assert_eq!(
            duplicate.add_page(Kind::Tool, None, vec![invalid, tool("same")], None),
            Err(Error::Protocol)
        );
        assert!(duplicate.finish().is_err());
    }
}
