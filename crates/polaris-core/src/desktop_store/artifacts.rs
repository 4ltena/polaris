//! Root-scoped immutable bodies for workflow artifacts.
//!
//! Session sidecars keep only graph metadata.  This store deliberately has no
//! garbage collection: a body can remain reachable from an older fork or a
//! recovery marker after its current session stops referring to it.

use super::{StoreError, StoreResult, disk::Directory};
use crate::workflow::{
    ArtifactHeaderV1, ArtifactRefV1, ChangeContractV1, FeatureSpecV1, IdeaDecisionV1,
    ImplementationPlanV1, MAX_WORKFLOW_ARTIFACT_BODY_BYTES, ProjectPrinciplesV1, TaskGraphV1,
    WorkflowArtifactGraphV1, WorkflowArtifactKindV1, validate_hash,
};
#[cfg(all(test, windows))]
use polaris_auth::protection::WindowsFamilyFile as File;
use polaris_desktop_protocol::ids::ProjectId;
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
#[cfg(not(all(test, windows)))]
use std::fs::File;
use std::{
    io::{Read, Write},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

const ARTIFACTS_DIRECTORY: &str = "workflow-artifacts-v1";
const OBJECTS_DIRECTORY: &str = "objects";
const TEMP_ATTEMPTS: usize = 32;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A project-scoped immutable body store below a verified desktop root.
///
/// `open_owned` is suitable for standalone recovery or migration code.  The
/// desktop writer should construct the store with the same already-pinned root
/// capability through `from_root`, so it never trusts a second pathname.
#[derive(Debug, Clone)]
pub struct WorkflowArtifactStore {
    objects: Arc<Directory>,
}

impl WorkflowArtifactStore {
    pub fn open_owned(root: &std::path::Path, project: &ProjectId) -> StoreResult<Self> {
        let root = Arc::new(Directory::open_owned(root)?);
        Self::from_root(root, project)
    }

    #[cfg(all(test, windows))]
    pub(super) fn from_root(_: Arc<Directory>, _: &ProjectId) -> StoreResult<Self> {
        Err(StoreError::UnsupportedPlatform)
    }

    #[cfg(not(all(test, windows)))]
    pub(super) fn from_root(root: Arc<Directory>, project: &ProjectId) -> StoreResult<Self> {
        root.verify()?;
        let artifacts = root.ensure_child(ARTIFACTS_DIRECTORY)?;
        let project = artifacts.ensure_child(&project_bucket(project))?;
        let objects = Arc::new(project.ensure_child(OBJECTS_DIRECTORY)?);
        objects.verify()?;
        Ok(Self { objects })
    }

    pub(super) fn inspect_from_root_m2a(
        root: Arc<Directory>,
        project: &ProjectId,
    ) -> StoreResult<Self> {
        root.verify()?;
        let artifacts = root.child(ARTIFACTS_DIRECTORY, false)?;
        let project = artifacts.child(&project_bucket(project), false)?;
        let objects = Arc::new(project.child(OBJECTS_DIRECTORY, false)?);
        objects.verify()?;
        Ok(Self { objects })
    }

    /// Write a canonical, validated body.  Repeating the exact same body is a
    /// successful no-op; a malformed or different pre-existing object is a
    /// recovery error and is never overwritten.
    pub fn put_change_contract(&self, contract: &ChangeContractV1) -> StoreResult<ArtifactRefV1> {
        self.put_typed(
            contract,
            ChangeContractV1::validate,
            ChangeContractV1::reference,
        )
    }

    /// Read and revalidate the exact bytes addressed by `reference`.
    pub fn read_change_contract(&self, reference: &ArtifactRefV1) -> StoreResult<ChangeContractV1> {
        self.read_typed(
            reference,
            ChangeContractV1::validate,
            ChangeContractV1::reference,
        )
    }

    pub fn put_project_principles(
        &self,
        principles: &ProjectPrinciplesV1,
    ) -> StoreResult<ArtifactRefV1> {
        self.put_typed(
            principles,
            ProjectPrinciplesV1::validate,
            ProjectPrinciplesV1::reference,
        )
    }

    pub fn read_project_principles(
        &self,
        graph: &WorkflowArtifactGraphV1,
        reference: &ArtifactRefV1,
    ) -> StoreResult<ProjectPrinciplesV1> {
        self.require_graph_member(graph, reference, WorkflowArtifactKindV1::ProjectPrinciples)?;
        let principles = self.read_typed(
            reference,
            ProjectPrinciplesV1::validate,
            ProjectPrinciplesV1::reference,
        )?;
        self.require_graph_header(graph, reference, &principles.header)?;
        Ok(principles)
    }

    pub fn put_idea_decision(&self, decision: &IdeaDecisionV1) -> StoreResult<ArtifactRefV1> {
        self.put_typed(
            decision,
            IdeaDecisionV1::validate,
            IdeaDecisionV1::reference,
        )
    }

    pub fn read_idea_decision(
        &self,
        graph: &WorkflowArtifactGraphV1,
        reference: &ArtifactRefV1,
    ) -> StoreResult<IdeaDecisionV1> {
        self.require_graph_member(graph, reference, WorkflowArtifactKindV1::IdeaDecision)?;
        let decision = self.read_typed(
            reference,
            IdeaDecisionV1::validate,
            IdeaDecisionV1::reference,
        )?;
        self.require_graph_header(graph, reference, &decision.header)?;
        Ok(decision)
    }

    /// Publish an immutable feature specification. The session graph advances
    /// only after this body is durable.
    pub fn put_feature_spec(&self, feature: &FeatureSpecV1) -> StoreResult<ArtifactRefV1> {
        self.put_typed(feature, FeatureSpecV1::validate, FeatureSpecV1::reference)
    }

    /// Read a feature specification only when its exact revision is reachable
    /// from the supplied validated graph.
    pub fn read_feature_spec(
        &self,
        graph: &WorkflowArtifactGraphV1,
        reference: &ArtifactRefV1,
    ) -> StoreResult<FeatureSpecV1> {
        self.require_graph_member(graph, reference, WorkflowArtifactKindV1::FeatureSpec)?;
        let feature =
            self.read_typed(reference, FeatureSpecV1::validate, FeatureSpecV1::reference)?;
        self.require_graph_header(graph, reference, &feature.header)?;
        Ok(feature)
    }

    pub fn put_implementation_plan(
        &self,
        plan: &ImplementationPlanV1,
    ) -> StoreResult<ArtifactRefV1> {
        self.put_typed(
            plan,
            ImplementationPlanV1::validate,
            ImplementationPlanV1::reference,
        )
    }

    pub fn read_implementation_plan(
        &self,
        graph: &WorkflowArtifactGraphV1,
        reference: &ArtifactRefV1,
    ) -> StoreResult<ImplementationPlanV1> {
        self.require_graph_member(graph, reference, WorkflowArtifactKindV1::ImplementationPlan)?;
        let plan = self.read_typed(
            reference,
            ImplementationPlanV1::validate,
            ImplementationPlanV1::reference,
        )?;
        self.require_graph_header(graph, reference, &plan.header)?;
        self.require_graph_member(
            graph,
            &plan.feature_spec,
            WorkflowArtifactKindV1::FeatureSpec,
        )?;
        Ok(plan)
    }

    pub fn put_task_graph(&self, task_graph: &TaskGraphV1) -> StoreResult<ArtifactRefV1> {
        self.put_typed(task_graph, TaskGraphV1::validate, TaskGraphV1::reference)
    }

    /// Read a task graph only after checking its feature and plan bodies.
    pub fn read_task_graph(
        &self,
        graph: &WorkflowArtifactGraphV1,
        reference: &ArtifactRefV1,
    ) -> StoreResult<TaskGraphV1> {
        self.require_graph_member(graph, reference, WorkflowArtifactKindV1::TaskGraph)?;
        let task_graph =
            self.read_typed(reference, TaskGraphV1::validate, TaskGraphV1::reference)?;
        self.require_graph_header(graph, reference, &task_graph.header)?;
        let feature = self.read_feature_spec(graph, &task_graph.feature_spec)?;
        let plan = self.read_implementation_plan(graph, &task_graph.implementation_plan)?;
        task_graph
            .validate_with_feature(&feature, &plan)
            .map_err(|_| StoreError::Corrupt("workflow task graph dependencies"))?;
        Ok(task_graph)
    }

    fn put_typed<T>(
        &self,
        artifact: &T,
        validate: impl Fn(&T) -> Result<(), String>,
        reference_of: impl Fn(&T) -> ArtifactRefV1,
    ) -> StoreResult<ArtifactRefV1>
    where
        T: Serialize + DeserializeOwned + PartialEq,
    {
        validate(artifact).map_err(|_| StoreError::Corrupt("workflow artifact body"))?;
        let reference = reference_of(artifact);
        validate_reference(&reference)?;
        let bytes = canonical_body_bytes(artifact)?;
        let name = object_name(&reference.content_hash)?;

        match self.read_typed_exact(&name, &reference, &validate, &reference_of) {
            Ok(existing) => {
                if existing == bytes {
                    return Ok(reference);
                }
                return Err(StoreError::Corrupt("workflow artifact object content"));
            }
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        let temporary = self.write_temporary(&bytes)?;
        let published = self.objects.link_no_replace(&temporary, &name)?;
        self.objects.unlink_file(&temporary)?;
        if published {
            self.objects.sync()?;
        }
        let existing = self.read_typed_exact(&name, &reference, &validate, &reference_of)?;
        if existing == bytes {
            Ok(reference)
        } else {
            Err(StoreError::Corrupt("workflow artifact object conflict"))
        }
    }

    fn read_typed<T>(
        &self,
        reference: &ArtifactRefV1,
        validate: impl Fn(&T) -> Result<(), String>,
        reference_of: impl Fn(&T) -> ArtifactRefV1,
    ) -> StoreResult<T>
    where
        T: Serialize + DeserializeOwned,
    {
        validate_reference(reference)?;
        let bytes = self.read_typed_exact(
            &object_name(&reference.content_hash)?,
            reference,
            &validate,
            &reference_of,
        )?;
        serde_json::from_slice(&bytes).map_err(StoreError::from)
    }

    fn read_typed_exact<T>(
        &self,
        name: &str,
        reference: &ArtifactRefV1,
        validate: &impl Fn(&T) -> Result<(), String>,
        reference_of: &impl Fn(&T) -> ArtifactRefV1,
    ) -> StoreResult<Vec<u8>>
    where
        T: Serialize + DeserializeOwned,
    {
        let bytes = self.read_exact(name)?;
        let artifact: T = serde_json::from_slice(&bytes)?;
        validate(&artifact).map_err(|_| StoreError::Corrupt("workflow artifact body"))?;
        if reference_of(&artifact) != *reference || canonical_body_bytes(&artifact)? != bytes {
            return Err(StoreError::Corrupt(
                "workflow artifact body/reference mismatch",
            ));
        }
        Ok(bytes)
    }

    fn require_graph_member(
        &self,
        graph: &WorkflowArtifactGraphV1,
        reference: &ArtifactRefV1,
        kind: WorkflowArtifactKindV1,
    ) -> StoreResult<()> {
        graph
            .validate()
            .map_err(|_| StoreError::Corrupt("workflow artifact graph"))?;
        validate_reference(reference)?;
        let metadata = graph
            .metadata(reference)
            .ok_or(StoreError::Corrupt("workflow artifact not in graph"))?;
        if metadata.kind != kind {
            return Err(StoreError::Corrupt("workflow artifact kind"));
        }
        Ok(())
    }

    fn require_graph_header(
        &self,
        graph: &WorkflowArtifactGraphV1,
        reference: &ArtifactRefV1,
        header: &ArtifactHeaderV1,
    ) -> StoreResult<()> {
        let metadata = graph
            .metadata(reference)
            .ok_or(StoreError::Corrupt("workflow artifact not in graph"))?;
        if metadata.header != *header {
            return Err(StoreError::Corrupt(
                "workflow artifact graph/header mismatch",
            ));
        }
        Ok(())
    }

    fn write_temporary(&self, bytes: &[u8]) -> StoreResult<String> {
        for _ in 0..TEMP_ATTEMPTS {
            let name = temporary_name();
            match self.objects.open(&name, true, true) {
                Ok(mut file) => {
                    let result = write_and_sync(&mut file, bytes);
                    drop(file);
                    match result {
                        Ok(()) => return Ok(name),
                        Err(error) => {
                            let _ = self.objects.unlink_file(&name);
                            return Err(error);
                        }
                    }
                }
                Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                }
                Err(error) => return Err(error),
            }
        }
        Err(StoreError::Busy)
    }

    fn read_exact(&self, name: &str) -> StoreResult<Vec<u8>> {
        let mut file = self.objects.open(name, false, false)?;
        let length = file.metadata()?.len();
        if length == 0 || length > MAX_WORKFLOW_ARTIFACT_BODY_BYTES as u64 {
            return Err(StoreError::Corrupt("workflow artifact object size"));
        }
        let mut bytes = Vec::with_capacity(length as usize);
        file.read_to_end(&mut bytes)?;
        if bytes.len() as u64 != length || bytes.len() > MAX_WORKFLOW_ARTIFACT_BODY_BYTES {
            return Err(StoreError::Corrupt("workflow artifact object size"));
        }
        Ok(bytes)
    }
}

/// Passive migration validation; no object creation or execution authority.
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
pub(in crate::desktop_store) fn validate_migration_object(
    name: &str,
    bytes: &[u8],
) -> StoreResult<()> {
    fn matches<T: Serialize + DeserializeOwned>(
        name: &str,
        bytes: &[u8],
        validate: impl Fn(&T) -> Result<(), String>,
        reference: impl Fn(&T) -> ArtifactRefV1,
    ) -> bool {
        let Ok(value) = serde_json::from_slice::<T>(bytes) else {
            return false;
        };
        validate(&value).is_ok()
            && object_name(&reference(&value).content_hash).ok().as_deref() == Some(name)
            && canonical_body_bytes(&value).ok().as_deref() == Some(bytes)
    }
    if matches(
        name,
        bytes,
        ChangeContractV1::validate,
        ChangeContractV1::reference,
    ) || matches(
        name,
        bytes,
        ProjectPrinciplesV1::validate,
        ProjectPrinciplesV1::reference,
    ) || matches(
        name,
        bytes,
        IdeaDecisionV1::validate,
        IdeaDecisionV1::reference,
    ) || matches(
        name,
        bytes,
        FeatureSpecV1::validate,
        FeatureSpecV1::reference,
    ) || matches(
        name,
        bytes,
        ImplementationPlanV1::validate,
        ImplementationPlanV1::reference,
    ) || matches(name, bytes, TaskGraphV1::validate, TaskGraphV1::reference)
    {
        Ok(())
    } else {
        Err(StoreError::Corrupt("workflow artifact migration body/name"))
    }
}

fn project_bucket(project: &ProjectId) -> String {
    format!("{:x}", Sha256::digest(project.as_str().as_bytes()))
}

fn object_name(hash: &str) -> StoreResult<String> {
    if !lowercase_sha256(hash) {
        return Err(StoreError::Corrupt("workflow artifact hash"));
    }
    Ok(format!("{hash}.json"))
}

fn validate_reference(reference: &ArtifactRefV1) -> StoreResult<()> {
    reference
        .validate()
        .map_err(|_| StoreError::Corrupt("workflow artifact reference"))?;
    if !lowercase_sha256(&reference.content_hash) {
        return Err(StoreError::Corrupt("workflow artifact hash"));
    }
    Ok(())
}

fn lowercase_sha256(value: &str) -> bool {
    validate_hash(value).is_ok()
        && value.len() == 64
        && value.bytes().all(|byte| {
            byte.is_ascii_digit() || (byte.is_ascii_lowercase() && byte.is_ascii_hexdigit())
        })
}

fn canonical_body_bytes(body: &impl Serialize) -> StoreResult<Vec<u8>> {
    let bytes = serde_json::to_vec(body)?;
    if bytes.is_empty() || bytes.len() > MAX_WORKFLOW_ARTIFACT_BODY_BYTES {
        return Err(StoreError::Corrupt("workflow artifact body size"));
    }
    Ok(bytes)
}

fn temporary_name() -> String {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(".artifact-{}-{sequence}.tmp", std::process::id())
}

fn write_and_sync(file: &mut File, bytes: &[u8]) -> StoreResult<()> {
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::workflow::{
        ArtifactStatusV1, DecisionAlternativeV1, IdeaDecisionOutcomeV1, IdeaDecisionV1,
        ImplementationPlanV1, ProjectPrincipleV1, ProjectPrinciplesV1, RequirementV1,
        RoutineEligibilityV1, TaskDefinitionV1, TaskGraphV1, TaskRuntimeV1, TaskStateV1,
        UnresolvedDecisionV1, VerificationCriterionV1, WorkflowArtifactGraphV1,
    };
    use std::fs;

    fn contract() -> ChangeContractV1 {
        ChangeContractV1::draft(
            "desktop-contract",
            RoutineEligibilityV1::local_component("desktop_store"),
            "a workflow transition needs a durable contract",
            "the session marker contains metadata only",
            "the body is content-addressed and revalidated",
            vec!["roundtrip the immutable body".into()],
        )
        .unwrap()
    }

    fn store() -> (tempfile::TempDir, WorkflowArtifactStore, ProjectId) {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let project = ProjectId::new("artifact-project").unwrap();
        let canonical = fs::canonicalize(root.path()).unwrap();
        let store = WorkflowArtifactStore::open_owned(&canonical, &project).unwrap();
        (root, store, project)
    }

    fn typed_artifacts() -> (FeatureSpecV1, ImplementationPlanV1, TaskGraphV1) {
        let feature = FeatureSpecV1::draft(
            "desktop-feature",
            "Desktop typed artifact storage",
            "Store exact typed bodies under the pinned desktop root.",
            vec![RequirementV1::stable("Typed bodies are graph reachable").unwrap()],
            vec![VerificationCriterionV1::stable("Typed body roundtrip succeeds", true).unwrap()],
        )
        .unwrap();
        let plan = ImplementationPlanV1::draft(
            "desktop-plan",
            feature.reference(),
            "Persist feature, implementation plan, and task graph before the marker advances.",
            vec!["desktop_store/artifacts.rs".into()],
        )
        .unwrap();
        let task_id = TaskDefinitionV1::stable_id("persist typed desktop bodies").unwrap();
        let task_graph = TaskGraphV1::draft(
            "desktop-task-graph",
            feature.reference(),
            plan.reference(),
            vec![TaskDefinitionV1 {
                id: task_id.clone(),
                title: "Persist typed desktop bodies".into(),
                scope: vec!["desktop_store/artifacts.rs".into()],
                requirement_ids: vec![feature.requirements[0].id.clone()],
                criterion_ids: vec![feature.acceptance_criteria[0].id.clone()],
                dependencies: Vec::new(),
            }],
            vec![TaskRuntimeV1 {
                task_id,
                state: TaskStateV1::Ready,
                blocker: None,
            }],
        )
        .unwrap();
        (feature, plan, task_graph)
    }

    #[test]
    fn roundtrip_and_idempotency_preserve_exact_bytes() {
        let (_root, store, _project) = store();
        let original = contract();
        let reference = store.put_change_contract(&original).unwrap();
        assert_eq!(store.put_change_contract(&original).unwrap(), reference);
        assert_eq!(store.read_change_contract(&reference).unwrap(), original);
    }

    #[test]
    fn tampered_or_oversized_objects_fail_closed() {
        use std::os::unix::fs::PermissionsExt;
        let (_root, store, _project) = store();
        let original = contract();
        let reference = store.put_change_contract(&original).unwrap();
        let path = store
            .objects
            .path()
            .join(object_name(&reference.content_hash).unwrap());
        fs::write(&path, br#"{"invalid":true}"#).unwrap();
        assert!(store.read_change_contract(&reference).is_err());

        let revised = contract().revised(ArtifactStatusV1::Ready).unwrap();
        let reference = revised.reference();
        let path = store
            .objects
            .path()
            .join(object_name(&reference.content_hash).unwrap());
        fs::write(&path, vec![b'x'; MAX_WORKFLOW_ARTIFACT_BODY_BYTES + 1]).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(store.read_change_contract(&reference).is_err());
    }

    #[test]
    fn symlink_and_hardlink_objects_are_rejected() {
        use std::os::unix::fs::symlink;
        let (root, store, _project) = store();
        let contract = contract();
        let reference = contract.reference();
        let path = store
            .objects
            .path()
            .join(object_name(&reference.content_hash).unwrap());
        let outside = root.path().join("outside.json");
        fs::write(&outside, serde_json::to_vec(&contract).unwrap()).unwrap();
        symlink(&outside, &path).unwrap();
        assert!(store.put_change_contract(&contract).is_err());
        fs::remove_file(&path).unwrap();
        fs::hard_link(&outside, &path).unwrap();
        assert!(store.put_change_contract(&contract).is_err());
    }

    #[test]
    fn concurrent_same_content_is_idempotent() {
        let (_root, store, _project) = store();
        let contract = contract();
        let left = store.clone();
        let right = store.clone();
        let left_contract = contract.clone();
        let left = std::thread::spawn(move || left.put_change_contract(&left_contract));
        let right = std::thread::spawn(move || right.put_change_contract(&contract));
        let left = left.join().unwrap().unwrap();
        let right = right.join().unwrap().unwrap();
        assert_eq!(left, right);
    }

    #[test]
    fn typed_bodies_need_exact_graph_membership_and_intact_dependencies() {
        let (_root, store, _project) = store();
        let (feature, plan, task_graph) = typed_artifacts();
        let feature_ref = store.put_feature_spec(&feature).unwrap();
        assert_eq!(store.put_feature_spec(&feature).unwrap(), feature_ref);
        let plan_ref = store.put_implementation_plan(&plan).unwrap();
        assert_eq!(store.put_implementation_plan(&plan).unwrap(), plan_ref);
        let tasks_ref = store.put_task_graph(&task_graph).unwrap();
        assert_eq!(store.put_task_graph(&task_graph).unwrap(), tasks_ref);

        let mut graph = WorkflowArtifactGraphV1::default();
        graph.insert_feature_spec(&feature).unwrap();
        graph.insert_implementation_plan(&plan).unwrap();
        graph.insert_task_graph(&task_graph).unwrap();

        assert_eq!(
            store.read_feature_spec(&graph, &feature_ref).unwrap(),
            feature
        );
        assert_eq!(
            store.read_implementation_plan(&graph, &plan_ref).unwrap(),
            plan
        );
        assert_eq!(
            store.read_task_graph(&graph, &tasks_ref).unwrap(),
            task_graph
        );
        assert!(store.read_task_graph(&graph, &plan_ref).is_err());
        assert!(
            store
                .read_feature_spec(&WorkflowArtifactGraphV1::default(), &feature_ref)
                .is_err()
        );

        let path = store
            .objects
            .path()
            .join(object_name(&feature_ref.content_hash).unwrap());
        fs::write(path, b"{}").unwrap();
        assert!(store.read_task_graph(&graph, &tasks_ref).is_err());
    }

    #[test]
    fn project_principles_and_unresolved_decision_are_exact_graph_bodies() {
        let (_root, store, _project) = store();
        let principles = ProjectPrinciplesV1::draft(
            "desktop-principles",
            vec![ProjectPrincipleV1 {
                id: "privacy".into(),
                statement: "Keep artifact bodies private.".into(),
                feature_ids: Vec::new(),
                task_ids: Vec::new(),
            }],
        )
        .unwrap();
        let decision = IdeaDecisionV1::draft(
            "desktop-decision",
            "Persist an unresolved decision.",
            Vec::new(),
            Vec::new(),
            vec![DecisionAlternativeV1 {
                id: "none".into(),
                description: "Do not persist it.".into(),
            }],
            vec!["Recovery needs durable state.".into()],
            IdeaDecisionOutcomeV1::Clarify,
            vec![UnresolvedDecisionV1 {
                id: "retention".into(),
                question: "When may it be removed?".into(),
            }],
        )
        .unwrap();
        let principles_ref = store.put_project_principles(&principles).unwrap();
        let decision_ref = store.put_idea_decision(&decision).unwrap();
        let mut graph = WorkflowArtifactGraphV1::default();
        graph.insert_project_principles(&principles).unwrap();
        graph.insert_idea_decision(&decision).unwrap();
        assert_eq!(
            store
                .read_project_principles(&graph, &principles_ref)
                .unwrap(),
            principles
        );
        assert_eq!(
            store.read_idea_decision(&graph, &decision_ref).unwrap(),
            decision
        );
        assert!(
            store
                .read_idea_decision(&WorkflowArtifactGraphV1::default(), &decision_ref)
                .is_err()
        );
    }
    #[test]
    fn migration_artifact_validates_all_six_canonical_bodies_and_exact_names() {
        let contract = contract();
        let (feature, plan, tasks) = typed_artifacts();
        let principles = ProjectPrinciplesV1::draft(
            "migration-principles",
            vec![ProjectPrincipleV1 {
                id: "privacy".into(),
                statement: "Keep bodies private".into(),
                feature_ids: Vec::new(),
                task_ids: Vec::new(),
            }],
        )
        .unwrap();
        let decision = IdeaDecisionV1::draft(
            "migration-decision",
            "Persist decision",
            Vec::new(),
            Vec::new(),
            vec![DecisionAlternativeV1 {
                id: "none".into(),
                description: "Keep existing".into(),
            }],
            vec!["Read-only validation".into()],
            IdeaDecisionOutcomeV1::Clarify,
            vec![UnresolvedDecisionV1 {
                id: "retention".into(),
                question: "When to remove?".into(),
            }],
        )
        .unwrap();
        macro_rules! check {
            ($body:expr) => {{
                let body = $body;
                let bytes = canonical_body_bytes(&body).unwrap();
                let name = object_name(&body.reference().content_hash).unwrap();
                validate_migration_object(&name, &bytes).unwrap();
                assert!(
                    validate_migration_object(&format!("{}.json", "f".repeat(64)), &bytes).is_err()
                );
                let mut noncanonical = bytes.clone();
                noncanonical.push(b'\n');
                assert!(validate_migration_object(&name, &noncanonical).is_err());
                assert!(validate_migration_object(&name, b"{}").is_err());
            }};
        }
        check!(contract);
        check!(feature);
        check!(plan);
        check!(tasks);
        check!(principles);
        check!(decision);
    }
}
