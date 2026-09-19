//! Trusted next-run role metadata configuration. Runtime binding happens only
//! during preparation of an already accepted run.
use super::*;
use polaris_core::desktop_store::{
    SavedRoleBinding, SavedRoleBindings, SavedRuntime, SavedTier, SavedTierBinding,
    SavedTierBindings, ToolSupport,
};
use polaris_desktop_protocol::role_bindings::{
    BoundRuntime, BoundTier, ObservedToolSupport, RoleBinding, RoleBindingsConfigured,
    RoleDescriptor, TierBinding,
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
                    runtime: to_wire_runtime(binding.runtime),
                    endpoint: binding.endpoint.clone(),
                    model: binding.model.clone(),
                    effort: binding.effort.clone(),
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

fn to_wire_runtime(runtime: SavedRuntime) -> BoundRuntime {
    match runtime {
        SavedRuntime::Ollama => BoundRuntime::Ollama,
        SavedRuntime::LmStudio => BoundRuntime::LmStudio,
        SavedRuntime::LlamaCpp => BoundRuntime::LlamaCpp,
        SavedRuntime::Account => BoundRuntime::Account,
    }
}

fn from_wire_runtime(runtime: BoundRuntime) -> SavedRuntime {
    match runtime {
        BoundRuntime::Ollama => SavedRuntime::Ollama,
        BoundRuntime::LmStudio => SavedRuntime::LmStudio,
        BoundRuntime::LlamaCpp => SavedRuntime::LlamaCpp,
        BoundRuntime::Account => SavedRuntime::Account,
    }
}

fn to_wire_tier(tier: SavedTier) -> BoundTier {
    match tier {
        SavedTier::Dwarf => BoundTier::Dwarf,
        SavedTier::Subgiant => BoundTier::Subgiant,
        SavedTier::Giant => BoundTier::Giant,
        SavedTier::Supergiant => BoundTier::Supergiant,
    }
}

fn from_wire_tier(tier: BoundTier) -> SavedTier {
    match tier {
        BoundTier::Dwarf => SavedTier::Dwarf,
        BoundTier::Subgiant => SavedTier::Subgiant,
        BoundTier::Giant => SavedTier::Giant,
        BoundTier::Supergiant => SavedTier::Supergiant,
    }
}

pub(super) fn tiers_to_wire(saved: Option<&SavedTierBindings>) -> Vec<TierBinding> {
    saved
        .map(|saved| {
            saved
                .bindings
                .iter()
                .map(|binding| TierBinding {
                    tier: to_wire_tier(binding.tier),
                    runtime: to_wire_runtime(binding.runtime),
                    endpoint: binding.endpoint.clone(),
                    model: binding.model.clone(),
                    effort: binding.effort.clone(),
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
            // An account model has no endpoint to check; a local one is
            // held to the same canonical form it always was.
            if let Some(given) = &binding.endpoint {
                let endpoint =
                    Endpoint::parse(given).map_err(|_| error(ErrorCode::InvalidRequest))?;
                if endpoint.as_str() != given {
                    return Err(error(ErrorCode::InvalidRequest));
                }
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
                runtime: from_wire_runtime(binding.runtime),
                endpoint: binding.endpoint.clone(),
                model: binding.model.clone(),
                effort: binding.effort.clone(),
                observed_tool_support: match binding.observed_tool_support {
                    ObservedToolSupport::Supported => ToolSupport::Supported,
                    ObservedToolSupport::Unsupported => ToolSupport::Unsupported,
                    ObservedToolSupport::Unknown => ToolSupport::Unknown,
                },
            });
        }
        let saved = SavedRoleBindings::new(saved).map_err(|_| error(ErrorCode::InvalidRequest))?;
        // An empty list is an explicit "price nothing", the same way an
        // empty role list is an explicit "bind nothing".
        let tiers = SavedTierBindings::new(
            params
                .tier_bindings
                .iter()
                .map(|binding| SavedTierBinding {
                    tier: from_wire_tier(binding.tier),
                    runtime: from_wire_runtime(binding.runtime),
                    endpoint: binding.endpoint.clone(),
                    model: binding.model.clone(),
                    effort: binding.effort.clone(),
                })
                .collect(),
        )
        .map_err(|_| error(ErrorCode::InvalidRequest))?;
        self.store
            .configure_role_bindings(
                params.expected_configuration_revision,
                Some(saved),
                Some(tiers),
                catalog,
            )
            .map_err(store_error)?;
        let state = self.store.snapshot().map_err(store_error)?.state;
        let configured = RoleBindingsConfigured {
            configuration_revision: state.configuration.configuration_revision,
            bindings: to_wire(state.role_bindings.as_ref()),
            tier_bindings: tiers_to_wire(state.tier_bindings.as_ref()),
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
    fn an_account_backed_role_is_saved_without_an_endpoint() {
        // The settings screen may name a model reached through the run's
        // own registration. It has no endpoint, and the canonical-origin
        // check that guards a local binding must not demand one.
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
                tier: polaris_skills::Tier::Subgiant,
                wall_seconds: 5,
                max_turns: 1,
                workflow_phase: None,
                continuation: false,
                output_schema: schema,
            }])),
        });
        let binding = RoleBinding {
            role: "reviewer".into(),
            runtime: BoundRuntime::Account,
            endpoint: None,
            model: "gpt-5.4-mini".into(),
            effort: Some("low".into()),
            observed_tool_support: ObservedToolSupport::Unknown,
        };
        let request = Request {
            protocol_version: Default::default(),
            client_id: client,
            request_id: RequestId::new("account-role").unwrap(),
            body: RequestBody::SessionRolesConfigure(
                engine.session.clone(),
                ConfigureRoleBindings {
                    expected_configuration_revision: DecimalU64::new(0),
                    bindings: vec![binding],
                    tier_bindings: Vec::new(),
                },
            ),
        };
        let mut events = vec![];
        let SuccessResult::SessionRolesConfigure(result) = engine
            .configure_role_bindings(&request, &mut events)
            .expect("an account model is a binding the store accepts")
        else {
            panic!("the result is the configuration that was saved");
        };
        assert_eq!(result.bindings.len(), 1);
        assert_eq!(result.bindings[0].runtime, BoundRuntime::Account);
        assert_eq!(result.bindings[0].endpoint, None);
        assert_eq!(result.bindings[0].effort.as_deref(), Some("low"));
    }

    #[test]
    fn weights_are_saved_alongside_roles_and_come_back_in_the_snapshot() {
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
            factory: Arc::new(CatalogFactory(vec![])),
        });
        let session = engine.session.clone();
        let request = |revision, tiers: Vec<TierBinding>| Request {
            protocol_version: Default::default(),
            client_id: client.clone(),
            request_id: RequestId::new("weights").unwrap(),
            body: RequestBody::SessionRolesConfigure(
                session.clone(),
                ConfigureRoleBindings {
                    expected_configuration_revision: DecimalU64::new(revision),
                    bindings: vec![],
                    tier_bindings: tiers,
                },
            ),
        };
        let priced = vec![
            TierBinding {
                tier: BoundTier::Supergiant,
                runtime: BoundRuntime::Account,
                endpoint: None,
                model: "gpt-6-astra".into(),
                effort: Some("xhigh".into()),
            },
            TierBinding {
                tier: BoundTier::Dwarf,
                runtime: BoundRuntime::Account,
                endpoint: None,
                model: "gpt-5.4-mini".into(),
                effort: None,
            },
        ];
        let mut events = vec![];
        let SuccessResult::SessionRolesConfigure(result) = engine
            .configure_role_bindings(&request(0, priced), &mut events)
            .expect("weights are saved with no role bound at all")
        else {
            panic!("the result is the configuration that was saved");
        };
        // Stored in weight order, so the file and the snapshot read the
        // same way every time regardless of the order they arrived in.
        assert_eq!(
            result
                .tier_bindings
                .iter()
                .map(|binding| binding.tier)
                .collect::<Vec<_>>(),
            vec![BoundTier::Dwarf, BoundTier::Supergiant]
        );
        assert_eq!(result.tier_bindings[1].effort.as_deref(), Some("xhigh"));
        assert_eq!(
            engine
                .store
                .snapshot()
                .unwrap()
                .state
                .tier_bindings
                .as_ref()
                .map(|saved| saved.bindings.len()),
            Some(2),
            "the store keeps them, not just the reply"
        );

        // Two entries for one weight would leave which model it buys up to
        // whichever survived.
        let twice = vec![
            TierBinding {
                tier: BoundTier::Dwarf,
                runtime: BoundRuntime::Account,
                endpoint: None,
                model: "a".into(),
                effort: None,
            },
            TierBinding {
                tier: BoundTier::Dwarf,
                runtime: BoundRuntime::Account,
                endpoint: None,
                model: "b".into(),
                effort: None,
            },
        ];
        assert!(
            engine
                .configure_role_bindings(&request(1, twice), &mut events)
                .is_err()
        );
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
                tier: polaris_skills::Tier::Subgiant,
                wall_seconds: 5,
                max_turns: 1,
                workflow_phase: None,
                continuation: false,
                output_schema: schema,
            }])),
        });
        let binding = RoleBinding {
            role: "reviewer".into(),
            runtime: BoundRuntime::Ollama,
            endpoint: Some("http://127.0.0.1:11434/".into()),
            effort: None,
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
                    tier_bindings: Vec::new(),
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
