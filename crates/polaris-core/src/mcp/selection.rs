//! Frozen MCP candidate ranking and bounded tool selection for one request.

use super::Error;
use crate::budget::{MCP_CANDIDATE_LIMIT, count_tokens, validate_mcp_tool_budget};
use polaris_mcp::{
    Connection,
    catalog::{Catalog, Item, ItemId, Kind, MAX_RUN_CATALOG_BYTES},
};
use polaris_tools::{McpSchema, ToolSpec};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;

/// Keep the connection, not just its catalog: every lookup checks notification,
/// TTL and connection validity even when the same view is retained by a caller.
pub struct CatalogView<'a> {
    sources: Vec<CatalogSource<'a>>,
}

enum CatalogSource<'a> {
    Connection(&'a Connection, u64),
    #[cfg(test)]
    Fixture(&'a str, &'a Catalog, Option<&'a std::cell::Cell<bool>>),
}

impl<'a> CatalogView<'a> {
    pub fn from_connections(connections: &[&'a Connection], revision: u64) -> Result<Self, Error> {
        if connections.len() > 4 {
            return Err(Error::ContextLimit);
        }
        let view = Self {
            sources: connections
                .iter()
                .map(|connection| CatalogSource::Connection(connection, revision))
                .collect(),
        };
        view.current()?;
        Ok(view)
    }

    #[cfg(test)]
    fn checked(catalogs: Vec<(&'a str, &'a Catalog)>) -> Result<Self, Error> {
        let view = Self {
            sources: catalogs
                .into_iter()
                .map(|(server, catalog)| CatalogSource::Fixture(server, catalog, None))
                .collect(),
        };
        view.current()?;
        Ok(view)
    }

    fn current(&self) -> Result<Vec<(&'a str, &'a Catalog)>, Error> {
        let catalogs = self
            .sources
            .iter()
            .map(|source| match source {
                CatalogSource::Connection(connection, revision) => Ok((
                    connection.server_id(),
                    connection
                        .catalog(*revision)
                        .map_err(|_| Error::CatalogUnavailable)?,
                )),
                #[cfg(test)]
                CatalogSource::Fixture(server, catalog, valid) => {
                    if valid.is_some_and(|valid| !valid.get()) {
                        Err(Error::CatalogUnavailable)
                    } else {
                        Ok((*server, *catalog))
                    }
                }
            })
            .collect::<Result<Vec<_>, Error>>()?;
        if catalogs.len() > 4 {
            return Err(Error::ContextLimit);
        }
        let mut servers = BTreeSet::new();
        let mut bytes = 0usize;
        for (server, catalog) in &catalogs {
            bytes = bytes
                .checked_add(catalog.encoded_bytes)
                .ok_or(Error::ContextLimit)?;
            if !servers.insert(*server) || bytes > MAX_RUN_CATALOG_BYTES {
                return Err(Error::ContextLimit);
            }
        }
        Ok(catalogs)
    }

    fn item(&self, id: &ItemId, generation: &str) -> Result<&Item, Error> {
        let (_, catalog) = self
            .current()?
            .into_iter()
            .find(|(server, _)| *server == id.server_id)
            .ok_or(Error::SelectionStale)?;
        if catalog.generation != generation {
            return Err(Error::SelectionStale);
        }
        catalog
            .items
            .iter()
            .find(|item| &item.id == id)
            .ok_or(Error::InvalidSelection)
    }

    /// Exact server/name search remains available independently of automatic
    /// ranking. Descriptions are data; they do not grant tools or change phases.
    pub fn candidates(
        &self,
        query: &str,
        server: Option<&str>,
        phase: &str,
    ) -> Result<Vec<Candidate>, Error> {
        Ok(self
            .candidates_matching(query, server, phase, None)?
            .candidates)
    }

    pub(crate) fn candidates_matching(
        &self,
        query: &str,
        server: Option<&str>,
        phase: &str,
        kind: Option<Kind>,
    ) -> Result<CandidateSearch, Error> {
        if query.len() > 4096 || server.is_some_and(|value| value.len() > 128) || phase.len() > 64 {
            return Err(Error::ContextLimit);
        }
        let query = query.to_lowercase();
        let words: Vec<_> = query
            .split(|c: char| !c.is_alphanumeric() && !"_.-/".contains(c))
            .filter(|word| !word.is_empty())
            .collect();
        let phase = phase.to_lowercase();
        let mut ranked = Vec::new();
        for (server_id, catalog) in &self.current()? {
            if server.is_some_and(|wanted| wanted != *server_id) {
                continue;
            }
            for item in &catalog.items {
                if kind.is_some_and(|kind| item.id.kind != kind) {
                    continue;
                }
                let name = item.name.to_lowercase();
                let key = item.id.key.to_lowercase();
                let description = item.description.to_lowercase();
                let exact = !query.is_empty() && (query == name || query == key);
                let name_matches = words
                    .iter()
                    .filter(|word| name.contains(**word) || key.contains(**word))
                    .count();
                let description_matches = words
                    .iter()
                    .filter(|word| description.contains(**word))
                    .count();
                let explicit_server = server.is_some() && query.is_empty();
                if !exact && name_matches == 0 && description_matches == 0 && !explicit_server {
                    continue;
                }
                let phase_match =
                    !phase.is_empty() && (name.contains(&phase) || description.contains(&phase));
                let score = (exact, name_matches, description_matches, phase_match);
                ranked.push((
                    score,
                    Candidate {
                        server_id: (*server_id).into(),
                        kind: item.id.kind,
                        name: item.name.clone(),
                        key: item.id.key.clone(),
                        description: item.description.chars().take(160).collect(),
                        generation: catalog.generation.clone(),
                    },
                ));
            }
        }
        ranked.sort_by(|(score_a, a), (score_b, b)| {
            score_b
                .cmp(score_a)
                .then_with(|| (&a.server_id, a.kind, &a.key).cmp(&(&b.server_id, b.kind, &b.key)))
        });
        let matched = ranked.len();
        let mut result = Vec::new();
        for (_, candidate) in ranked {
            if result.len() == 12 {
                break;
            }
            result.push(candidate);
            if count_tokens(&serde_json::to_string(&result).map_err(|_| Error::ContextLimit)?)
                > MCP_CANDIDATE_LIMIT
            {
                result.pop();
            }
        }
        Ok(CandidateSearch {
            truncated: result.len() < matched,
            candidates: result,
        })
    }

    /// Live identity for a preselected tool; `None` when its summary or schema
    /// changed since the index was written.
    pub(crate) fn preselected(
        &self,
        pick: &super::preselection::Pick,
    ) -> Result<Option<(ItemId, String)>, Error> {
        let (_, catalog) = self
            .current()?
            .into_iter()
            .find(|(server, _)| *server == pick.server_id)
            .ok_or(Error::CatalogUnavailable)?;
        let Some(item) = catalog
            .items
            .iter()
            .find(|item| item.id.kind == Kind::Tool && item.id.key == pick.key)
        else {
            return Ok(None);
        };
        let description = polaris_mcp::catalog::index::summary_description(&item.description);
        let same = polaris_mcp::catalog::index::summary_hash(
            &pick.server_id,
            &item.id.key,
            &item.name,
            &description,
        )
        .is_ok_and(|hash| hash == pick.summary_hash)
            && item.schema_hash.as_deref() == Some(&pick.schema_hash);
        Ok(same.then(|| (item.id.clone(), catalog.generation.clone())))
    }

    pub fn rejected_items(&self) -> Result<usize, Error> {
        Ok(self
            .current()?
            .iter()
            .map(|(_, catalog)| catalog.rejected_items)
            .sum())
    }
}

pub(crate) struct CandidateSearch {
    pub candidates: Vec<Candidate>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub server_id: String,
    pub kind: Kind,
    pub name: String,
    pub key: String,
    pub description: String,
    pub generation: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SelectedIdentity {
    pub id: ItemId,
    pub catalog_generation: String,
    pub schema_hash: String,
    pub alias: String,
}

struct Selected {
    identity: SelectedIdentity,
    spec: ToolSpec,
    input: McpSchema,
    // Compile output as well: a malformed declaration quarantines this item.
    _output: Option<McpSchema>,
}

/// A meta-tool selection applies to exactly the next provider request. Selecting
/// while processing an answer cannot change that answer's frozen dispatch set.
#[derive(Default)]
pub struct PendingSelection {
    selected: Vec<Selected>,
}

impl PendingSelection {
    pub(crate) fn references_server(&self, server: &str) -> bool {
        self.selected
            .iter()
            .any(|s| s.identity.id.server_id == server)
    }

    pub fn select(
        &mut self,
        view: &CatalogView<'_>,
        id: &ItemId,
        generation: &str,
    ) -> Result<(), Error> {
        let item = view.item(id, generation)?;
        if id.kind != Kind::Tool {
            return Err(Error::InvalidSelection);
        }
        if self
            .selected
            .iter()
            .any(|selected| &selected.identity.id == id)
        {
            return Err(Error::InvalidSelection);
        }
        if self.selected.len() == 4 {
            return Err(Error::ContextLimit);
        }
        let spec = ToolSpec {
            name: item.alias.clone().ok_or(Error::InvalidSelection)?,
            description: item.description.clone(),
            parameters: item.input_schema.clone().ok_or(Error::InvalidSchema)?,
        };
        let mut specs: Vec<_> = self
            .selected
            .iter()
            .map(|selected| selected.spec.clone())
            .collect();
        specs.push(spec.clone());
        validate_mcp_tool_budget(&specs).map_err(|_| Error::ContextLimit)?;
        // Never compile an entire catalog, or a schema beyond the wire budget.
        let input = McpSchema::compile(&spec.parameters).map_err(|_| Error::InvalidSchema)?;
        let output = item
            .output_schema
            .as_ref()
            .map(McpSchema::compile)
            .transpose()
            .map_err(|_| Error::InvalidSchema)?;
        self.selected.push(Selected {
            identity: SelectedIdentity {
                id: id.clone(),
                catalog_generation: generation.into(),
                schema_hash: item.schema_hash.clone().ok_or(Error::InvalidSchema)?,
                alias: spec.name.clone(),
            },
            spec,
            input,
            _output: output,
        });
        Ok(())
    }

    pub fn freeze(&mut self, view: &CatalogView<'_>) -> Result<RequestSnapshot, Error> {
        // Drain even on staleness: an old selection cannot reappear after a
        // reconnect or later request. The model must explicitly select again.
        let selected = std::mem::take(&mut self.selected);
        view.current()?;
        for selection in &selected {
            verify_identity(view, &selection.identity)?;
        }
        Ok(RequestSnapshot { selected })
    }
}

#[derive(Default)]
pub struct RequestSnapshot {
    selected: Vec<Selected>,
}

impl RequestSnapshot {
    pub(crate) fn servers(&self) -> BTreeSet<String> {
        self.selected
            .iter()
            .map(|s| s.identity.id.server_id.clone())
            .collect()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.selected
            .iter()
            .map(|selected| selected.spec.clone())
            .collect()
    }

    /// Only resolves identity and validates arguments. A separate operation
    /// owner must persist intent and authorize the exact call before sending.
    pub fn resolve(
        &self,
        fresh_view: &CatalogView<'_>,
        alias: &str,
        arguments: &Value,
    ) -> Result<&SelectedIdentity, Error> {
        let selected = self
            .selected
            .iter()
            .find(|selected| selected.identity.alias == alias)
            .ok_or(Error::InvalidSelection)?;
        verify_identity(fresh_view, &selected.identity)?;
        selected
            .input
            .validate(arguments)
            .map_err(|_| Error::InvalidArguments)?;
        Ok(&selected.identity)
    }
}

fn verify_identity(view: &CatalogView<'_>, identity: &SelectedIdentity) -> Result<(), Error> {
    let item = view.item(&identity.id, &identity.catalog_generation)?;
    if item.schema_hash.as_ref() != Some(&identity.schema_hash)
        || item.alias.as_ref() != Some(&identity.alias)
    {
        return Err(Error::SelectionStale);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
