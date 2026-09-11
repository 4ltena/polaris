//! One owned metadata query. No Writer or provider generation crosses this boundary.
use super::*;
use polaris_desktop_protocol::local_models::{
    LocalAvailability, LocalModel, LocalModelsResult, LocalProvider, ObservedCapability,
    ObservedExecutionLocation, ObservedLoadState,
};
use polaris_provider::local::{
    Capability as ProviderCapability, Endpoint, ExecutionLocation, LoadState, LocalAdapter,
    LocalError, Runtime,
};
const MAX_INSPECTED_MODELS: usize = 32;

pub(super) struct InventoryJob {
    request: Request,
    generation: u64,
    worker: Option<std::thread::JoinHandle<Result<LocalModelsResult, ()>>>,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
}
impl InventoryJob {
    fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}
impl Drop for InventoryJob {
    fn drop(&mut self) {
        // Emergency blocking-owner destruction only. Normal loops cancel/poll.
        self.cancel();
        if let Some(worker) = self.worker.take()
            && let Err(payload) = worker.join()
        {
            std::mem::forget(payload);
        }
    }
}
impl Engine {
    pub(super) fn start_local_models(&mut self, request: &Request) -> Result<(), ProtocolError> {
        validate_request(self.connection, request).map_err(|_| error(ErrorCode::InvalidRequest))?;
        let RequestBody::LocalModels(session, params) = &request.body else {
            return Err(error(ErrorCode::InvalidRequest));
        };
        if self.client.as_ref() != Some(&request.client_id) || session != &self.session {
            return Err(error(ErrorCode::PermissionDenied));
        }
        if !self.production {
            return Err(error(ErrorCode::CapabilityUnavailable));
        }
        if self.failed {
            return Err(error(ErrorCode::StorageFailed));
        }
        if self.draining
            || self.disconnected.load(Ordering::Acquire)
            || self.active.is_some()
            || self.local_models.is_some()
        {
            return Err(error(ErrorCode::SessionBusy));
        }
        let endpoint =
            Endpoint::parse(&params.endpoint).map_err(|_| error(ErrorCode::InvalidRequest))?;
        if endpoint.as_str() != params.endpoint {
            return Err(error(ErrorCode::InvalidRequest));
        }
        let provider = params.provider;
        let runtime = match provider {
            LocalProvider::Ollama => Runtime::Ollama,
            LocalProvider::Lmstudio => Runtime::LmStudio,
        };
        let canonical = params.endpoint.clone();
        let generation = self
            .inventory_generation
            .checked_add(1)
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?;
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        let worker = std::thread::Builder::new().name("polaris-inventory".into()).spawn(move || {
            let runtime_owner = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|_| ())?;
            runtime_owner.block_on(async move {
                tokio::select! {
                    biased;
                    _ = cancelled => Err(()),
                    result = async {
                        let adapter = match LocalAdapter::new(runtime, endpoint) {
                            Ok(adapter) => adapter,
                            Err(_) => return Ok(LocalModelsResult { provider, endpoint: canonical, availability: LocalAvailability::InvalidResponse, models: vec![] }),
                        };
                        let observed = match adapter.inventory_with_load_state().await {
                            Ok(observed) => observed,
                            Err(LocalError::Transport | LocalError::Timeout) => return Ok(LocalModelsResult { provider, endpoint: canonical, availability: LocalAvailability::Unreachable, models: vec![] }),
                            Err(_) => return Ok(LocalModelsResult { provider, endpoint: canonical, availability: LocalAvailability::InvalidResponse, models: vec![] }),
                        };
                        // Ollama's list omits capabilities. Enrich a bounded
                        // prefix with metadata-only show calls under one total
                        // deadline; unobserved entries remain explicitly Unknown.
                        let mut details = std::collections::HashMap::new();
                        if runtime == Runtime::Ollama {
                            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                            for model in observed.iter().filter(|model| model.execution_location() != ExecutionLocation::Remote).take(MAX_INSPECTED_MODELS) {
                                match tokio::time::timeout_at(deadline, adapter.select(model)).await {
                                    Ok(Ok(selection)) => { details.insert(model.id().to_owned(), selection); }
                                    Ok(Err(_)) => {}
                                    Err(_) => break,
                                }
                            }
                        }
                        let models = observed.into_iter().map(|model| {
                            let selected = details.get(model.id()).map(|selection| selection.model()).unwrap_or(&model);
                            LocalModel {
                            model_id: model.id().to_owned(),
                            // A successful Ollama show observation deliberately
                            // drops the earlier list digest; the adapter forbids
                            // combining non-atomic identity and capability reads.
                            digest: selected.digest().map(str::to_owned),
                            variant: selected.variant().map(str::to_owned),
                            max_context_length: selected.max_context_length().map(DecimalU64::new),
                            completion: capability(selected.capabilities().completion),
                            tools: capability(selected.capabilities().tools),
                            vision: capability(selected.capabilities().vision),
                            reasoning: capability(selected.capabilities().reasoning),
                            execution_location: match selected.execution_location() {
                                ExecutionLocation::Remote => ObservedExecutionLocation::Remote,
                                ExecutionLocation::Unknown => ObservedExecutionLocation::Unknown,
                            },
                            load_state: match selected.load_state() {
                                LoadState::Loaded => ObservedLoadState::Loaded,
                                LoadState::Unloaded => ObservedLoadState::Unloaded,
                                LoadState::Unknown => ObservedLoadState::Unknown,
                            },
                        }}).collect();
                        Ok(LocalModelsResult { provider, endpoint: canonical, availability: LocalAvailability::Available, models })
                    } => result,
                }
            })
        }).map_err(|_| error(ErrorCode::CapabilityUnavailable))?;
        self.inventory_generation = generation;
        self.local_models = Some(InventoryJob {
            request: request.clone(),
            generation,
            worker: Some(worker),
            cancel: Some(cancel),
        });
        Ok(())
    }
    pub(super) fn poll_local_models(
        &mut self,
        out: Option<&Outbox>,
        stopping: bool,
    ) -> Result<(), ServiceError> {
        let Some(job) = &mut self.local_models else {
            return Ok(());
        };
        let suppress =
            stopping || self.draining || self.failed || self.disconnected.load(Ordering::Acquire);
        if suppress {
            job.cancel();
            // Invalidate once, including while cancellation is still joining.
            if job.generation == self.inventory_generation {
                self.inventory_generation = self.inventory_generation.saturating_add(1);
            }
        }
        if !job
            .worker
            .as_ref()
            .is_some_and(|worker| worker.is_finished())
        {
            return Ok(());
        }
        let mut job = self.local_models.take().expect("finished inventory job");
        let outcome = match job.worker.take().expect("finished worker").join() {
            Ok(result) => result,
            Err(payload) => {
                std::mem::forget(payload);
                Err(())
            }
        };
        if suppress || job.generation != self.inventory_generation {
            return Ok(());
        }
        let result = outcome
            .map(SuccessResult::LocalModels)
            .map_err(|_| error(ErrorCode::CapabilityUnavailable));
        let mut response =
            Response::for_request(&job.request, result).map_err(|_| ServiceError::Worker)?;
        // Bound the complete envelope, including JSON escaping and correlation IDs.
        if polaris_desktop_protocol::codec::encode(&response).is_err() {
            response =
                Response::for_request(&job.request, Err(error(ErrorCode::CapabilityUnavailable)))
                    .map_err(|_| ServiceError::Worker)?;
        }
        if !self.disconnected.load(Ordering::Acquire)
            && let Some(out) = out
        {
            out.send(&response)?;
        }
        Ok(())
    }
}

fn capability(value: ProviderCapability) -> ObservedCapability {
    match value {
        ProviderCapability::Supported => ObservedCapability::Supported,
        ProviderCapability::Unsupported => ObservedCapability::Unsupported,
        ProviderCapability::Unknown => ObservedCapability::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        time::{Duration, Instant},
    };

    fn fixture(endpoint: &str) -> (PrototypeRoot, Engine, Request) {
        let root = PrototypeRoot::new().unwrap();
        let mut engine = Engine::create(&root, Options::default()).unwrap();
        engine.production = true;
        let request = Request {
            protocol_version: Default::default(),
            client_id: ClientId::new("inventory-client").unwrap(),
            request_id: RequestId::new("inventory-request").unwrap(),
            body: RequestBody::LocalModels(
                engine.session.clone(),
                polaris_desktop_protocol::local_models::LocalModels {
                    provider: LocalProvider::Ollama,
                    endpoint: endpoint.into(),
                },
            ),
        };
        engine.connection = ConnectionState::Ready;
        engine.client = Some(request.client_id.clone());
        (root, engine, request)
    }
    fn until(mut done: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < end, "inventory worker did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    #[test]
    fn local_models_actual_inventory_is_metadata_only_and_single_owned() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let (_root, mut engine, request) = fixture(&endpoint);
        let before = engine.store.snapshot().unwrap();
        engine.start_local_models(&request).unwrap();
        assert_eq!(
            engine.start_local_models(&request).unwrap_err().code,
            ErrorCode::SessionBusy
        );
        // The owner remains available while network I/O belongs to the worker.
        engine.snapshot(false).unwrap();
        let mut stream = None;
        until(|| match listener.accept() {
            Ok((socket, _)) => {
                stream = Some(socket);
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(error) => panic!("{error}"),
        });
        let mut stream = stream.unwrap();
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = [0u8; 4096];
        let count = stream.read(&mut bytes).unwrap();
        assert!(
            std::str::from_utf8(&bytes[..count])
                .unwrap()
                .starts_with("GET /api/tags HTTP/1.1\r\n")
        );
        let body = r#"{"models":[{"name":"exact:tag","digest":"sha256:observed"}]}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
        drop(stream);
        let mut running = None;
        until(|| match listener.accept() {
            Ok((socket, _)) => {
                running = Some(socket);
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(error) => panic!("{error}"),
        });
        let mut running = running.unwrap();
        running.set_nonblocking(false).unwrap();
        running
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let count = running.read(&mut bytes).unwrap();
        assert!(
            std::str::from_utf8(&bytes[..count])
                .unwrap()
                .starts_with("GET /api/ps HTTP/1.1\r\n")
        );
        let body = r#"{"models":[{"name":"exact:tag","model":"exact:tag"}]}"#;
        write!(
            running,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
        drop(running);
        let mut show = None;
        until(|| match listener.accept() {
            Ok((socket, _)) => {
                show = Some(socket);
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(error) => panic!("{error}"),
        });
        let mut show = show.unwrap();
        show.set_nonblocking(false).unwrap();
        show.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let count = show.read(&mut bytes).unwrap();
        assert!(
            std::str::from_utf8(&bytes[..count])
                .unwrap()
                .starts_with("POST /api/show HTTP/1.1\r\n")
        );
        let body =
            r#"{"capabilities":["completion","tools"],"model_info":{"qwen.context_length":32768}}"#;
        write!(
            show,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
        drop(show);
        until(|| {
            engine
                .local_models
                .as_ref()
                .unwrap()
                .worker
                .as_ref()
                .unwrap()
                .is_finished()
        });
        let mut job = engine.local_models.take().unwrap();
        let result = job.worker.take().unwrap().join().unwrap().unwrap();
        assert_eq!(result.endpoint, endpoint);
        assert_eq!(
            result.models,
            vec![LocalModel {
                model_id: "exact:tag".into(),
                digest: None,
                variant: None,
                max_context_length: Some(DecimalU64::new(32768)),
                completion: ObservedCapability::Supported,
                tools: ObservedCapability::Supported,
                vision: ObservedCapability::Unsupported,
                reasoning: ObservedCapability::Unsupported,
                execution_location: ObservedExecutionLocation::Unknown,
                load_state: ObservedLoadState::Loaded,
            }]
        );
        assert_eq!(
            serde_json::to_value(engine.store.snapshot().unwrap().state).unwrap(),
            serde_json::to_value(before.state).unwrap()
        );
    }
    #[test]
    fn local_models_disconnect_cancels_joins_and_invalidates_without_save() {
        let (_root, mut engine, request) = fixture("http://127.0.0.1:11434/");
        let before = engine.store.snapshot().unwrap();
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let observed = finished.clone();
        engine.inventory_generation = 1;
        engine.local_models = Some(InventoryJob {
            request,
            generation: 1,
            cancel: Some(cancel),
            worker: Some(std::thread::spawn(move || {
                let _ = cancelled.blocking_recv();
                observed.store(true, Ordering::Release);
                Err(())
            })),
        });
        engine.disconnected.store(true, Ordering::Release);
        until(|| {
            engine.poll_local_models(None, false).unwrap();
            engine.local_models.is_none()
        });
        assert!(finished.load(Ordering::Acquire));
        assert_eq!(engine.inventory_generation, 2);
        assert_eq!(
            serde_json::to_value(engine.store.snapshot().unwrap().state).unwrap(),
            serde_json::to_value(before.state).unwrap()
        );
    }
    #[test]
    fn local_models_rejects_untrusted_origins_and_disabled_runtime_without_worker() {
        for endpoint in [
            "http://example.com/",
            "http://localhost:11434/",
            "http://127.0.0.1:11434",
            "http://user@127.0.0.1/",
        ] {
            let (_root, mut engine, request) = fixture(endpoint);
            assert_eq!(
                engine.start_local_models(&request).unwrap_err().code,
                ErrorCode::InvalidRequest
            );
            assert!(engine.local_models.is_none());
        }
        let (_root, mut engine, request) = fixture("http://127.0.0.1:11434/");
        engine.production = false;
        assert_eq!(
            engine.start_local_models(&request).unwrap_err().code,
            ErrorCode::CapabilityUnavailable
        );
        assert!(engine.local_models.is_none());
    }
}
