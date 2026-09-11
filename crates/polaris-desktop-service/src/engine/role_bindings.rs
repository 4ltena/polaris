//! Trusted next-run role metadata configuration. Runtime binding happens only
//! during preparation of an already accepted run.
use super::*;
use polaris_core::desktop_store::{SavedRoleBinding, SavedRoleBindings, SavedRuntime, ToolSupport};
use polaris_desktop_protocol::{
    local_models::LocalProvider,
    role_bindings::{ObservedToolSupport, RoleBinding, RoleBindingsConfigured, RoleDescriptor},
};
use polaris_provider::local::Endpoint;

pub(super) fn to_wire(saved: Option<&SavedRoleBindings>) -> Vec<RoleBinding> {
    saved
        .map(|saved| {
            saved
                .bindings
                .iter()
                .map(|binding| RoleBinding {
                    role: binding.role.clone(),
                    provider: match binding.runtime {
                        SavedRuntime::Ollama => LocalProvider::Ollama,
                        SavedRuntime::LmStudio => LocalProvider::Lmstudio,
                    },
                    endpoint: binding.endpoint.clone(),
                    model: binding.model.clone(),
                    observed_tool_support: match binding.observed_tool_support {
                        ToolSupport::Supported => ObservedToolSupport::Supported,
                        ToolSupport::Unsupported => ObservedToolSupport::Unsupported,
                        ToolSupport::Unknown => ObservedToolSupport::Unknown,
                    },
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn catalog_to_wire(catalog: &[polaris_skills::AgentType]) -> Vec<RoleDescriptor> {
    catalog
        .iter()
        .map(|agent| RoleDescriptor {
            role: agent.name.clone(),
            requires_tools: !agent.allowed_tools.is_empty(),
        })
        .collect()
}

#[cfg(target_os = "macos")]
impl Engine {
    pub(super) fn role_catalog_wire(&self) -> Vec<RoleDescriptor> {
        self.trusted_runs
            .as_ref()
            .map(|runs| catalog_to_wire(runs.factory.role_catalog()))
            .unwrap_or_default()
    }

    pub(super) fn configure_role_bindings(
        &mut self,
        request: &Request,
        events: &mut Vec<EventBody>,
    ) -> Result<SuccessResult, ProtocolError> {
        let RequestBody::SessionRolesConfigure(_, params) = &request.body else {
            return Err(error(ErrorCode::InvalidRequest));
        };
        let factory = self
            .trusted_runs
            .as_ref()
            .map(|runs| runs.factory.clone())
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?;
        let catalog = factory.role_catalog();
        let mut saved = Vec::with_capacity(params.bindings.len());
        for binding in &params.bindings {
            let endpoint =
                Endpoint::parse(&binding.endpoint).map_err(|_| error(ErrorCode::InvalidRequest))?;
            if endpoint.as_str() != binding.endpoint {
                return Err(error(ErrorCode::InvalidRequest));
            }
            let agent = catalog
                .iter()
                .find(|agent| agent.name == binding.role)
                .ok_or_else(|| error(ErrorCode::InvalidRequest))?;
            if !agent.allowed_tools.is_empty()
                && binding.observed_tool_support != ObservedToolSupport::Supported
            {
                return Err(error(ErrorCode::CapabilityUnavailable));
            }
            saved.push(SavedRoleBinding {
                role: binding.role.clone(),
                runtime: match binding.provider {
                    LocalProvider::Ollama => SavedRuntime::Ollama,
                    LocalProvider::Lmstudio => SavedRuntime::LmStudio,
                },
                endpoint: binding.endpoint.clone(),
                model: binding.model.clone(),
                observed_tool_support: match binding.observed_tool_support {
                    ObservedToolSupport::Supported => ToolSupport::Supported,
                    ObservedToolSupport::Unsupported => ToolSupport::Unsupported,
                    ObservedToolSupport::Unknown => ToolSupport::Unknown,
                },
            });
        }
        let saved = SavedRoleBindings::new(saved).map_err(|_| error(ErrorCode::InvalidRequest))?;
        self.store
            .configure_role_bindings(params.expected_configuration_revision, Some(saved), catalog)
            .map_err(store_error)?;
        let state = self.store.snapshot().map_err(store_error)?.state;
        let configured = RoleBindingsConfigured {
            configuration_revision: state.configuration.configuration_revision,
            bindings: to_wire(state.role_bindings.as_ref()),
        };
        events.push(EventBody::RoleBindingsUpdated(configured.clone()));
        Ok(SuccessResult::SessionRolesConfigure(configured))
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::{TrustedRunFactory, TrustedRunInputs};
    use polaris_core::desktop_store::Published;
    use polaris_desktop_protocol::{
        ids::{ClientId, RequestId},
        role_bindings::ConfigureRoleBindings,
    };
    use std::sync::Arc;

    struct CatalogFactory(Vec<polaris_skills::AgentType>);
    impl TrustedRunFactory for CatalogFactory {
        fn role_catalog(&self) -> &[polaris_skills::AgentType] {
            &self.0
        }
        fn prepare(&self, _: &RunTarget, _: &Published) -> Result<TrustedRunInputs, ServiceError> {
            panic!("role configuration must not prepare a run")
        }
    }

    #[test]
    fn role_configuration_is_cas_persisted_and_catalog_validated() {
        let root = PrototypeRoot::new().unwrap();
        let mut engine = Engine::create(&root, Options::default()).unwrap();
        engine.production = true;
        engine.connection = ConnectionState::Ready;
        let client = ClientId::new("client").unwrap();
        engine.client = Some(client.clone());
        let configuration = engine.store.snapshot().unwrap().state.configuration;
        let directory = tempfile::tempdir().unwrap();
        let schema = directory.path().join("schema.json");
        std::fs::write(&schema, "{}").unwrap();
        engine.trusted_runs = Some(real::TrustedRuns {
            configuration,
            factory: Arc::new(CatalogFactory(vec![polaris_skills::AgentType {
                name: "reviewer".into(),
                description: "fixture".into(),
                body: "fixture".into(),
                path: directory.path().to_owned(),
                allowed_tools: vec![],
                access: polaris_skills::AgentAccess::Read,
                tier: "medium".into(),
                wall_seconds: 5,
                max_turns: 1,
                workflow_phase: None,
                continuation: false,
                output_schema: schema,
            }])),
        });
        let binding = RoleBinding {
            role: "reviewer".into(),
            provider: LocalProvider::Ollama,
            endpoint: "http://127.0.0.1:11434/".into(),
            model: "qwen:small".into(),
            observed_tool_support: ObservedToolSupport::Unknown,
        };
        let request = Request {
            protocol_version: Default::default(),
            client_id: client,
            request_id: RequestId::new("role-request").unwrap(),
            body: RequestBody::SessionRolesConfigure(
                engine.session.clone(),
                ConfigureRoleBindings {
                    expected_configuration_revision: DecimalU64::new(0),
                    bindings: vec![binding.clone()],
                },
            ),
        };
        let mut events = vec![];
        let SuccessResult::SessionRolesConfigure(result) = engine
            .configure_role_bindings(&request, &mut events)
            .unwrap()
        else {
            panic!("wrong response")
        };
        assert_eq!(result.configuration_revision, DecimalU64::new(1));
        assert_eq!(result.bindings, vec![binding]);
        assert!(matches!(
            events.as_slice(),
            [EventBody::RoleBindingsUpdated(_)]
        ));
        let saved = engine.store.snapshot().unwrap();
        assert_eq!(
            saved.state.configuration.configuration_revision,
            DecimalU64::new(1)
        );
        assert_eq!(
            saved.state.role_bindings.unwrap().bindings[0].role,
            "reviewer"
        );

        assert_eq!(
            engine
                .configure_role_bindings(&request, &mut vec![])
                .unwrap_err()
                .code,
            ErrorCode::RevisionConflict,
        );
    }
}

#[cfg(not(target_os = "macos"))]
impl Engine {
    pub(super) fn role_catalog_wire(&self) -> Vec<RoleDescriptor> {
        vec![]
    }

    pub(super) fn configure_role_bindings(
        &mut self,
        _request: &Request,
        _events: &mut Vec<EventBody>,
    ) -> Result<SuccessResult, ProtocolError> {
        Err(error(ErrorCode::CapabilityUnavailable))
    }
}
