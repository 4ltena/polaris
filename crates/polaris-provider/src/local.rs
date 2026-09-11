//! Bounded, metadata-only discovery of explicitly configured local runtimes.
//!
//! No inference, process management, downloads, or credential discovery occurs here.
//! A loopback transport does not establish where the runtime executes a model.

use reqwest::{Client, Url};
use serde_json::{Map, Value};
use std::{collections::HashSet, net::IpAddr, time::Duration};

pub const MAX_INVENTORY_BYTES: usize = 1024 * 1024;
pub const MAX_MODELS: usize = 1024;
const MAX_ID_BYTES: usize = 512;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const LOAD_STATE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    Ollama,
    LmStudio,
}

/// Validated IP-literal loopback origin, without a path or embedded credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint(Url);

impl Endpoint {
    pub fn parse(input: &str) -> Result<Self, LocalError> {
        if input.len() > 256
            || input.chars().any(|c| c.is_whitespace() || c.is_control())
            || input.contains(['@', '?', '#', '%', '\\'])
        {
            return Err(LocalError::InvalidEndpoint);
        }
        let rest = input
            .strip_prefix("http://")
            .or_else(|| input.strip_prefix("https://"))
            .ok_or(LocalError::InvalidEndpoint)?;
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        if authority.contains('/') {
            return Err(LocalError::InvalidEndpoint);
        }
        // Validate the original spelling before WHATWG URL parsing normalizes IPv4.
        let host = if let Some(v6) = authority.strip_prefix('[') {
            v6.split_once(']').ok_or(LocalError::InvalidEndpoint)?.0
        } else {
            authority
                .split(':')
                .next()
                .ok_or(LocalError::InvalidEndpoint)?
        };
        let ip: IpAddr = host.parse().map_err(|_| LocalError::InvalidEndpoint)?;
        if !ip.is_loopback() {
            return Err(LocalError::InvalidEndpoint);
        }
        let url = Url::parse(input).map_err(|_| LocalError::InvalidEndpoint)?;
        if url.port_or_known_default().is_none_or(|p| p == 0) {
            return Err(LocalError::InvalidEndpoint);
        }
        Ok(Self(url))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub completion: Capability,
    pub tools: Capability,
    pub vision: Capability,
    pub reasoning: Capability,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            completion: Capability::Unknown,
            tools: Capability::Unknown,
            vision: Capability::Unknown,
            reasoning: Capability::Unknown,
        }
    }
}

/// Discovery never claims local execution, including for models without remote fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionLocation {
    Remote,
    Unknown,
}

/// A runtime's observation of whether a downloaded model is in memory.
///
/// It is metadata for presentation only.  It does not authorize a load, prove
/// future availability, or establish that a concurrent inventory is atomic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadState {
    Loaded,
    Unloaded,
    Unknown,
}

/// Immutable observation, tied to the exact runtime and endpoint that supplied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelMetadata {
    runtime: Runtime,
    endpoint: Endpoint,
    id: String,
    digest: Option<String>,
    variant: Option<String>,
    max_context_length: Option<u64>,
    capabilities: Capabilities,
    execution_location: ExecutionLocation,
    load_state: LoadState,
}

impl ModelMetadata {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn digest(&self) -> Option<&str> {
        self.digest.as_deref()
    }
    pub fn variant(&self) -> Option<&str> {
        self.variant.as_deref()
    }
    pub fn max_context_length(&self) -> Option<u64> {
        self.max_context_length
    }
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }
    pub fn execution_location(&self) -> ExecutionLocation {
        self.execution_location
    }
    pub fn load_state(&self) -> LoadState {
        self.load_state
    }
}

/// A fixed metadata snapshot, not permission to infer or proof of current availability.
/// Ollama selections describe only the show observation: the requested name is not
/// a content identity, and no inventory digest/variant/capabilities are carried over.
/// LM Studio selections describe the supplied native inventory observation.
/// Execution settings, slot ownership and revalidation belong to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelection {
    model: ModelMetadata,
}

impl ModelSelection {
    pub fn runtime(&self) -> Runtime {
        self.model.runtime
    }
    pub fn endpoint(&self) -> &Endpoint {
        &self.model.endpoint
    }
    pub fn model(&self) -> &ModelMetadata {
        &self.model
    }
}

/// Errors deliberately exclude response bodies, input URLs and transport diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    #[error("invalid loopback endpoint")]
    InvalidEndpoint,
    #[error("could not construct local client")]
    Client,
    #[error("local metadata request failed")]
    Transport,
    #[error("local metadata request timed out")]
    Timeout,
    #[error("local metadata HTTP status {0}")]
    HttpStatus(u16),
    #[error("local metadata response exceeds byte limit")]
    ResponseTooLarge,
    #[error("invalid local model metadata")]
    InvalidMetadata,
    #[error("model belongs to a different endpoint or runtime")]
    SelectionMismatch,
}

pub struct LocalAdapter {
    runtime: Runtime,
    endpoint: Endpoint,
    client: Client,
}

impl LocalAdapter {
    pub fn new(runtime: Runtime, endpoint: Endpoint) -> Result<Self, LocalError> {
        // Do not use the shared builder: it may read SSL_CERT_FILE. This adapter
        // discovers no credentials, CA files or proxy settings from the environment.
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| LocalError::Client)?;
        Ok(Self {
            runtime,
            endpoint,
            client,
        })
    }

    /// List downloaded/runtime-visible models without an extra residency probe.
    /// Execution callers use this baseline observation and never pay for UI
    /// presentation metadata.
    pub async fn inventory(&self) -> Result<Vec<ModelMetadata>, LocalError> {
        self.inventory_impl(false).await
    }

    /// List models together with bounded, presentation-only residency metadata.
    /// This method never loads, unloads, downloads, or performs inference.
    pub async fn inventory_with_load_state(&self) -> Result<Vec<ModelMetadata>, LocalError> {
        self.inventory_impl(true).await
    }

    async fn inventory_impl(
        &self,
        include_load_state: bool,
    ) -> Result<Vec<ModelMetadata>, LocalError> {
        let route = match self.runtime {
            Runtime::Ollama => "/api/tags",
            Runtime::LmStudio => "/api/v1/models",
        };
        let value = self.request(route, None).await?;
        let models = value
            .get("models")
            .and_then(Value::as_array)
            .ok_or(LocalError::InvalidMetadata)?;
        if models.len() > MAX_MODELS {
            return Err(LocalError::InvalidMetadata);
        }
        let mut seen = HashSet::new();
        let mut models: Vec<_> = models
            .iter()
            .map(|value| {
                let obj = object(value)?;
                let key = match self.runtime {
                    Runtime::Ollama => "name",
                    Runtime::LmStudio => "key",
                };
                let id = text(obj, key)?.ok_or(LocalError::InvalidMetadata)?;
                if !seen.insert(id.clone()) {
                    return Err(LocalError::InvalidMetadata);
                }
                let mut model = ModelMetadata {
                    runtime: self.runtime,
                    endpoint: self.endpoint.clone(),
                    id,
                    digest: text(obj, "digest")?,
                    variant: text(obj, "selected_variant")?,
                    max_context_length: None,
                    capabilities: Capabilities::default(),
                    execution_location: location(obj)?,
                    load_state: if include_load_state {
                        load_state(self.runtime, obj)?
                    } else {
                        LoadState::Unknown
                    },
                };
                apply_metadata(self.runtime, obj, &mut model)?;
                Ok(model)
            })
            .collect::<Result<_, _>>()?;
        if include_load_state {
            self.apply_load_states(&mut models).await;
        }
        Ok(models)
    }

    /// A failed, malformed, or unavailable load-state observation must never
    /// turn an otherwise valid inventory into "unloaded".  Keep Unknown.
    async fn apply_load_states(&self, models: &mut [ModelMetadata]) {
        match self.runtime {
            Runtime::Ollama => {
                let Ok(Ok(value)) =
                    tokio::time::timeout(LOAD_STATE_TIMEOUT, self.request("/api/ps", None)).await
                else {
                    return;
                };
                let Ok(loaded) = running_ollama_models(&value) else {
                    return;
                };
                for model in models {
                    model.load_state = if loaded.contains(model.id()) {
                        LoadState::Loaded
                    } else {
                        LoadState::Unloaded
                    };
                }
            }
            Runtime::LmStudio => {}
        }
    }

    /// Select an observed model. Ollama's show request is metadata-only (despite POST).
    /// LM Studio already includes native capabilities in the inventory response.
    pub async fn select(&self, observed: &ModelMetadata) -> Result<ModelSelection, LocalError> {
        if observed.runtime != self.runtime || observed.endpoint != self.endpoint {
            return Err(LocalError::SelectionMismatch);
        }
        let mut model = observed.clone();
        if self.runtime == Runtime::Ollama {
            // show(name) may observe different content from the earlier inventory.
            // It supplies no digest; even bracketing it with tags cannot bind the
            // observations atomically. Keep only the requested scope and prior
            // remote evidence, never combine identity A with capabilities B.
            model = ModelMetadata {
                runtime: observed.runtime,
                endpoint: observed.endpoint.clone(),
                id: observed.id.clone(),
                digest: None,
                variant: None,
                max_context_length: None,
                capabilities: Capabilities::default(),
                execution_location: observed.execution_location,
                // `show` has no load-state field. Keep the separately
                // observed `/api/ps` result as presentation metadata only.
                load_state: observed.load_state,
            };
            let value = self
                .request("/api/show", Some(serde_json::json!({"model": observed.id})))
                .await?;
            let obj = object(&value)?;
            // An empty or unrelated JSON object is not a successful show response.
            if !obj.contains_key("details")
                && !obj.contains_key("model_info")
                && !obj.contains_key("capabilities")
            {
                return Err(LocalError::InvalidMetadata);
            }
            if location(obj)? == ExecutionLocation::Remote {
                model.execution_location = ExecutionLocation::Remote;
            }
            apply_metadata(self.runtime, obj, &mut model)?;
        }
        Ok(ModelSelection { model })
    }

    async fn request(&self, route: &str, body: Option<Value>) -> Result<Value, LocalError> {
        let mut url = self.endpoint.0.clone();
        url.set_path(route);
        let request = match body {
            Some(body) => self.client.post(url).json(&body),
            None => self.client.get(url),
        };
        // Outer deadline includes body consumption; trickling bytes cannot extend it.
        tokio::time::timeout(REQUEST_TIMEOUT, async {
            let mut response = request.send().await.map_err(transport_error)?;
            if !response.status().is_success() {
                return Err(LocalError::HttpStatus(response.status().as_u16()));
            }
            if response
                .content_length()
                .is_some_and(|n| n > MAX_INVENTORY_BYTES as u64)
            {
                return Err(LocalError::ResponseTooLarge);
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
                if chunk.len() > MAX_INVENTORY_BYTES - bytes.len() {
                    return Err(LocalError::ResponseTooLarge);
                }
                bytes.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&bytes).map_err(|_| LocalError::InvalidMetadata)
        })
        .await
        .map_err(|_| LocalError::Timeout)?
    }
}

fn transport_error(error: reqwest::Error) -> LocalError {
    if error.is_timeout() {
        LocalError::Timeout
    } else {
        LocalError::Transport
    }
}

fn object(value: &Value) -> Result<&Map<String, Value>, LocalError> {
    value.as_object().ok_or(LocalError::InvalidMetadata)
}

fn text(obj: &Map<String, Value>, key: &str) -> Result<Option<String>, LocalError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s))
            if !s.is_empty() && s.len() <= MAX_ID_BYTES && !s.chars().any(char::is_control) =>
        {
            Ok(Some(s.clone()))
        }
        _ => Err(LocalError::InvalidMetadata),
    }
}

fn location(obj: &Map<String, Value>) -> Result<ExecutionLocation, LocalError> {
    let mut remote = false;
    for key in ["remote_host", "remote_model"] {
        match obj.get(key) {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) => remote |= !s.is_empty(),
            _ => return Err(LocalError::InvalidMetadata),
        }
    }
    Ok(if remote {
        ExecutionLocation::Remote
    } else {
        ExecutionLocation::Unknown
    })
}

fn load_state(runtime: Runtime, obj: &Map<String, Value>) -> Result<LoadState, LocalError> {
    match runtime {
        // `/api/tags` does not carry memory residency. `/api/ps` is queried
        // after the bounded inventory succeeds.
        Runtime::Ollama => Ok(LoadState::Unknown),
        // LM Studio v1 documents `loaded_instances` on every model.  Older
        // servers and malformed values remain Unknown rather than becoming an
        // unsupported runtime or an invented unloaded state.
        Runtime::LmStudio => match obj.get("loaded_instances") {
            None | Some(Value::Null) => Ok(LoadState::Unknown),
            Some(Value::Array(instances)) if instances.len() <= MAX_MODELS => {
                Ok(if instances.is_empty() {
                    LoadState::Unloaded
                } else if instances.iter().all(valid_lmstudio_instance) {
                    LoadState::Loaded
                } else {
                    LoadState::Unknown
                })
            }
            Some(_) => Ok(LoadState::Unknown),
        },
    }
}

fn valid_lmstudio_instance(value: &Value) -> bool {
    let Ok(instance) = object(value) else {
        return false;
    };
    let Ok(Some(_)) = text(instance, "id") else {
        return false;
    };
    let Some(config) = instance.get("config") else {
        return false;
    };
    let Ok(config) = object(config) else {
        return false;
    };
    config
        .get("context_length")
        .is_some_and(|value| positive(value).is_ok())
}

fn running_ollama_models(value: &Value) -> Result<HashSet<String>, LocalError> {
    let models = value
        .get("models")
        .and_then(Value::as_array)
        .filter(|models| models.len() <= MAX_MODELS)
        .ok_or(LocalError::InvalidMetadata)?;
    let mut loaded = HashSet::new();
    for value in models {
        let obj = object(value)?;
        // The documented response has both `name` and `model`; use the model
        // name which is also the `/api/tags` inventory key.  An empty or
        // malformed running entry invalidates this extra observation only.
        let id = text(obj, "name")?.ok_or(LocalError::InvalidMetadata)?;
        if !loaded.insert(id) {
            return Err(LocalError::InvalidMetadata);
        }
    }
    Ok(loaded)
}

fn positive(value: &Value) -> Result<u64, LocalError> {
    value
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or(LocalError::InvalidMetadata)
}

fn bool_cap(obj: &Map<String, Value>, key: &str) -> Result<Capability, LocalError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(Capability::Unknown),
        Some(Value::Bool(true)) => Ok(Capability::Supported),
        Some(Value::Bool(false)) => Ok(Capability::Unsupported),
        _ => Err(LocalError::InvalidMetadata),
    }
}

fn apply_metadata(
    runtime: Runtime,
    obj: &Map<String, Value>,
    model: &mut ModelMetadata,
) -> Result<(), LocalError> {
    match runtime {
        Runtime::Ollama => {
            if let Some(value) = obj.get("capabilities").filter(|v| !v.is_null()) {
                let list = value.as_array().ok_or(LocalError::InvalidMetadata)?;
                if list.len() > 64
                    || list
                        .iter()
                        .any(|v| v.as_str().is_none_or(|s| s.len() > MAX_ID_BYTES))
                {
                    return Err(LocalError::InvalidMetadata);
                }
                let cap = |name: &str| {
                    if list.iter().any(|v| v.as_str() == Some(name)) {
                        Capability::Supported
                    } else {
                        Capability::Unsupported
                    }
                };
                model.capabilities = Capabilities {
                    completion: cap("completion"),
                    tools: cap("tools"),
                    vision: cap("vision"),
                    reasoning: cap("thinking"),
                };
            }
            if let Some(info) = obj.get("model_info").filter(|v| !v.is_null()) {
                let info = object(info)?;
                // Multiple architecture entries are conservatively bounded by the minimum.
                for (key, value) in info {
                    if key.ends_with(".context_length") {
                        let n = positive(value)?;
                        model.max_context_length =
                            Some(model.max_context_length.map_or(n, |old| old.min(n)));
                    }
                }
            }
        }
        Runtime::LmStudio => {
            model.capabilities.completion = match text(obj, "type")?.as_deref() {
                Some("llm") => Capability::Supported,
                Some("embedding") => Capability::Unsupported,
                _ => Capability::Unknown,
            };
            if let Some(value) = obj.get("max_context_length").filter(|v| !v.is_null()) {
                model.max_context_length = Some(positive(value)?);
            }
            if let Some(value) = obj.get("capabilities").filter(|v| !v.is_null()) {
                let caps = object(value)?;
                model.capabilities.tools = bool_cap(caps, "trained_for_tool_use")?;
                model.capabilities.vision = bool_cap(caps, "vision")?;
                // Reasoning controls are runtime-specific; presence alone is not support.
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "local/tests.rs"]
mod tests;
