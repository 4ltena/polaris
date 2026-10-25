//! Private immutable bodies for the durable workflow artifact graph.
//!
//! The session marker contains only validated metadata.  A body is published
//! first, under a project-scoped content-addressed namespace, and the caller
//! may then atomically advance its session marker while holding that session's
//! writer lock.  An interrupted publication can therefore leave an orphaned
//! body, but can never expose a graph reference without its body.

use crate::workflow::{
    ArtifactHeaderV1, ArtifactRefV1, ChangeContractV1, FeatureSpecV1, IdeaDecisionV1,
    ImplementationPlanV1, MAX_WORKFLOW_ARTIFACT_BODY_BYTES, ProjectPrinciplesV1, TaskGraphV1,
    WorkflowArtifactGraphV1, WorkflowArtifactKindV1, validate_hash,
};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(unix)]
const STORE_DIRECTORY: &str = "workflow-artifacts-v1";
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct WorkflowArtifactStore {
    project_directory: PathBuf,
}

impl WorkflowArtifactStore {
    #[cfg(unix)]
    pub fn open(data_root: &Path, project_id: &str) -> io::Result<Self> {
        if project_id.trim().is_empty() || project_id.contains('\0') {
            return Err(invalid("Invalid project ID"));
        }
        let root = data_root.join(STORE_DIRECTORY);
        ensure_private_directory(&root)?;
        let project_hash = crate::conversation_state::content_hash(project_id.as_bytes());
        validate_hash(&project_hash).map_err(invalid_string)?;
        let project_directory = root.join(project_hash);
        ensure_private_directory(&project_directory)?;
        Ok(Self { project_directory })
    }

    #[cfg(not(unix))]
    pub fn open(_data_root: &Path, _project_id: &str) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Cannot verify artifact storage protections on this platform",
        ))
    }

    /// Publishes exactly one immutable contract body.  Repeating the same body
    /// is idempotent; a byte-different object at the same content address is
    /// corruption and is never overwritten.
    pub fn put_change_contract(&self, contract: &ChangeContractV1) -> io::Result<ArtifactRefV1> {
        contract.validate().map_err(invalid_string)?;
        let reference = contract.reference();
        validate_hash(&reference.content_hash).map_err(invalid_string)?;
        let bytes = serde_json::to_vec(contract)?;
        self.publish_exact(&reference.content_hash, &bytes, contract)?;
        Ok(reference)
    }

    /// Loads only a body whose exact immutable reference is in the supplied
    /// current graph.  Callers cannot use this store as an arbitrary project
    /// file reader.
    pub fn load_change_contract(
        &self,
        graph: &WorkflowArtifactGraphV1,
        reference: &ArtifactRefV1,
    ) -> io::Result<ChangeContractV1> {
        graph.validate().map_err(invalid_string)?;
        reference.validate().map_err(invalid_string)?;
        let metadata = graph
            .metadata(reference)
            .ok_or_else(|| invalid("The artifact reference is missing from the current graph"))?;
        if metadata.kind != WorkflowArtifactKindV1::ChangeContract {
            return Err(invalid("The artifact is not a change contract"));
        }
        let path = self.object_path(&reference.content_hash)?;
        let bytes = read_private_regular_file(&path)?;
        let contract: ChangeContractV1 = serde_json::from_slice(&bytes)?;
        contract.validate().map_err(invalid_string)?;
        if contract.reference() != *reference
            || contract.header != metadata.header
            || contract.eligibility != metadata.eligibility
        {
            return Err(invalid("Change contract body and graph metadata mismatch"));
        }
        Ok(contract)
    }

    pub fn put_project_principles(
        &self,
        principles: &ProjectPrinciplesV1,
    ) -> io::Result<ArtifactRefV1> {
        self.put_typed_artifact(
            principles,
            ProjectPrinciplesV1::validate,
            ProjectPrinciplesV1::reference,
        )
    }

    /// Loads the exact project-scoped principles body already selected by a
    /// session graph. The caller may then filter it for an active task.
    pub fn load_project_principles(
        &self,
        graph: &WorkflowArtifactGraphV1,
        reference: &ArtifactRefV1,
    ) -> io::Result<ProjectPrinciplesV1> {
        self.load_typed_artifact(
            graph,
            reference,
            WorkflowArtifactKindV1::ProjectPrinciples,
            ProjectPrinciplesV1::validate,
            ProjectPrinciplesV1::reference,
            |artifact| &artifact.header,
        )
    }

    pub fn put_idea_decision(&self, decision: &IdeaDecisionV1) -> io::Result<ArtifactRefV1> {
        self.put_typed_artifact(
            decision,
            IdeaDecisionV1::validate,
            IdeaDecisionV1::reference,
        )
    }

    /// Loads an exact session-scoped decision body, including unresolved
    /// questions. It grants no transition authority.
    pub fn load_idea_decision(
        &self,
        graph: &WorkflowArtifactGraphV1,
        reference: &ArtifactRefV1,
    ) -> io::Result<IdeaDecisionV1> {
        self.load_typed_artifact(
            graph,
            reference,
            WorkflowArtifactKindV1::IdeaDecision,
            IdeaDecisionV1::validate,
            IdeaDecisionV1::reference,
            |artifact| &artifact.header,
        )
    }

    /// Publishes an immutable feature specification body. The graph marker is
    /// deliberately advanced by the caller only after this succeeds.
    pub fn put_feature_spec(&self, feature: &FeatureSpecV1) -> io::Result<ArtifactRefV1> {
        self.put_typed_artifact(feature, FeatureSpecV1::validate, FeatureSpecV1::reference)
    }

    /// Loads one exact feature specification that is already reachable from
    /// `graph`; this store cannot retrieve arbitrary CAS objects.
    pub fn load_feature_spec(
        &self,
        graph: &WorkflowArtifactGraphV1,
        reference: &ArtifactRefV1,
    ) -> io::Result<FeatureSpecV1> {
        self.load_typed_artifact(
            graph,
            reference,
            WorkflowArtifactKindV1::FeatureSpec,
            FeatureSpecV1::validate,
            FeatureSpecV1::reference,
            |artifact| &artifact.header,
        )
    }

    /// Publishes an immutable implementation plan body.
    pub fn put_implementation_plan(
        &self,
        plan: &ImplementationPlanV1,
    ) -> io::Result<ArtifactRefV1> {
        self.put_typed_artifact(
            plan,
            ImplementationPlanV1::validate,
            ImplementationPlanV1::reference,
        )
    }

    /// Loads one exact implementation plan whose feature reference is already
    /// present in the supplied graph.
    pub fn load_implementation_plan(
        &self,
        graph: &WorkflowArtifactGraphV1,
        reference: &ArtifactRefV1,
    ) -> io::Result<ImplementationPlanV1> {
        let plan = self.load_typed_artifact(
            graph,
            reference,
            WorkflowArtifactKindV1::ImplementationPlan,
            ImplementationPlanV1::validate,
            ImplementationPlanV1::reference,
            |artifact| &artifact.header,
        )?;
        let feature = graph.metadata(&plan.feature_spec).ok_or_else(|| {
            invalid("The implementation plan's feature spec is missing from the graph")
        })?;
        if feature.kind != WorkflowArtifactKindV1::FeatureSpec {
            return Err(invalid(
                "The implementation plan does not reference a feature spec",
            ));
        }
        Ok(plan)
    }

    /// Publishes an immutable task graph body.
    pub fn put_task_graph(&self, graph: &TaskGraphV1) -> io::Result<ArtifactRefV1> {
        self.put_typed_artifact(graph, TaskGraphV1::validate, TaskGraphV1::reference)
    }

    /// Loads one exact task graph and verifies its referenced feature and plan
    /// bodies as well. This makes dangling or semantically mismatched task
    /// references fail closed even when graph metadata itself is well formed.
    pub fn load_task_graph(
        &self,
        graph: &WorkflowArtifactGraphV1,
        reference: &ArtifactRefV1,
    ) -> io::Result<TaskGraphV1> {
        let task_graph = self.load_typed_artifact(
            graph,
            reference,
            WorkflowArtifactKindV1::TaskGraph,
            TaskGraphV1::validate,
            TaskGraphV1::reference,
            |artifact| &artifact.header,
        )?;
        let feature = self.load_feature_spec(graph, &task_graph.feature_spec)?;
        let plan = self.load_implementation_plan(graph, &task_graph.implementation_plan)?;
        task_graph
            .validate_with_feature(&feature, &plan)
            .map_err(invalid_string)?;
        Ok(task_graph)
    }

    fn put_typed_artifact<T>(
        &self,
        artifact: &T,
        validate: impl Fn(&T) -> Result<(), String>,
        reference: impl Fn(&T) -> ArtifactRefV1,
    ) -> io::Result<ArtifactRefV1>
    where
        T: Serialize + DeserializeOwned + PartialEq,
    {
        validate(artifact).map_err(invalid_string)?;
        let reference = reference(artifact);
        validate_hash(&reference.content_hash).map_err(invalid_string)?;
        let bytes = serde_json::to_vec(artifact)?;
        self.publish_typed_exact(&reference.content_hash, &bytes, artifact, validate)?;
        Ok(reference)
    }

    fn load_typed_artifact<T>(
        &self,
        graph: &WorkflowArtifactGraphV1,
        reference: &ArtifactRefV1,
        expected_kind: WorkflowArtifactKindV1,
        validate: impl Fn(&T) -> Result<(), String>,
        artifact_reference: impl Fn(&T) -> ArtifactRefV1,
        header: impl Fn(&T) -> &ArtifactHeaderV1,
    ) -> io::Result<T>
    where
        T: DeserializeOwned,
    {
        graph.validate().map_err(invalid_string)?;
        reference.validate().map_err(invalid_string)?;
        let metadata = graph
            .metadata(reference)
            .ok_or_else(|| invalid("The artifact reference is missing from the current graph"))?;
        if metadata.kind != expected_kind {
            return Err(invalid("Artifact kind does not match the expected kind"));
        }
        let path = self.object_path(&reference.content_hash)?;
        let bytes = read_private_regular_file(&path)?;
        let artifact: T = serde_json::from_slice(&bytes)?;
        validate(&artifact).map_err(invalid_string)?;
        if artifact_reference(&artifact) != *reference || header(&artifact) != &metadata.header {
            return Err(invalid("Artifact body and graph metadata mismatch"));
        }
        Ok(artifact)
    }

    fn publish_exact(
        &self,
        content_hash: &str,
        bytes: &[u8],
        contract: &ChangeContractV1,
    ) -> io::Result<()> {
        let path = self.object_path(content_hash)?;
        if path.exists() {
            return self.check_exact_existing(&path, bytes, contract);
        }

        let temporary = self.project_directory.join(format!(
            ".pending-{}-{}",
            std::process::id(),
            NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = private_create_new(&temporary)?;
        if let Err(error) = (|| -> io::Result<()> {
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })() {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        drop(file);

        match fs::hard_link(&temporary, &path) {
            Ok(()) => {
                // The final name exists before the temporary link is removed;
                // validate after removal so durable objects always have one link.
                fs::remove_file(&temporary)?;
                sync_directory(&self.project_directory)?;
                self.check_exact_existing(&path, bytes, contract)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&temporary);
                self.check_exact_existing(&path, bytes, contract)
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                Err(error)
            }
        }
    }

    fn publish_typed_exact<T>(
        &self,
        content_hash: &str,
        bytes: &[u8],
        artifact: &T,
        validate: impl Fn(&T) -> Result<(), String>,
    ) -> io::Result<()>
    where
        T: DeserializeOwned + PartialEq,
    {
        let path = self.object_path(content_hash)?;
        if path.exists() {
            return self.check_typed_existing(&path, bytes, artifact, validate);
        }

        let temporary = self.project_directory.join(format!(
            ".pending-{}-{}",
            std::process::id(),
            NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = private_create_new(&temporary)?;
        if let Err(error) = (|| -> io::Result<()> {
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })() {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        drop(file);

        match fs::hard_link(&temporary, &path) {
            Ok(()) => {
                fs::remove_file(&temporary)?;
                sync_directory(&self.project_directory)?;
                self.check_typed_existing(&path, bytes, artifact, validate)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&temporary);
                self.check_typed_existing(&path, bytes, artifact, validate)
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                Err(error)
            }
        }
    }

    fn check_exact_existing(
        &self,
        path: &Path,
        expected: &[u8],
        contract: &ChangeContractV1,
    ) -> io::Result<()> {
        let bytes = read_private_regular_file(path)?;
        if bytes != expected {
            return Err(invalid("Different bodies for the same artifact hash"));
        }
        let parsed: ChangeContractV1 = serde_json::from_slice(&bytes)?;
        parsed.validate().map_err(invalid_string)?;
        if &parsed != contract {
            return Err(invalid("Saved change contract mismatch"));
        }
        Ok(())
    }

    fn check_typed_existing<T>(
        &self,
        path: &Path,
        expected: &[u8],
        artifact: &T,
        validate: impl Fn(&T) -> Result<(), String>,
    ) -> io::Result<()>
    where
        T: DeserializeOwned + PartialEq,
    {
        let bytes = read_private_regular_file(path)?;
        if bytes != expected {
            return Err(invalid("Different bodies for the same artifact hash"));
        }
        let parsed: T = serde_json::from_slice(&bytes)?;
        validate(&parsed).map_err(invalid_string)?;
        if &parsed != artifact {
            return Err(invalid("Saved artifact body mismatch"));
        }
        Ok(())
    }

    fn object_path(&self, content_hash: &str) -> io::Result<PathBuf> {
        validate_hash(content_hash).map_err(invalid_string)?;
        Ok(self.project_directory.join(format!("{content_hash}.json")))
    }

    #[cfg(test)]
    fn object_path_for_test(&self, reference: &ArtifactRefV1) -> PathBuf {
        self.object_path(&reference.content_hash).unwrap()
    }
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> io::Result<()> {
    if path.exists() {
        validate_private_directory(path)
    } else {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(path) {
            Ok(()) => {
                sync_directory(
                    path.parent()
                        .ok_or_else(|| invalid("Invalid artifact storage location"))?,
                )?;
                validate_private_directory(path)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                validate_private_directory(path)
            }
            Err(error) => Err(error),
        }
    }
}

fn private_create_new(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    validate_private_regular_file(path)?;
    Ok(file)
}

fn read_private_regular_file(path: &Path) -> io::Result<Vec<u8>> {
    validate_private_regular_file(path)?;
    if fs::symlink_metadata(path)?.len() > MAX_WORKFLOW_ARTIFACT_BODY_BYTES as u64 {
        return Err(invalid("Artifact body exceeds the limit"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(path)?;
    let length = file.metadata()?.len();
    if length > MAX_WORKFLOW_ARTIFACT_BODY_BYTES as u64 {
        return Err(invalid("Artifact body exceeds the limit"));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    std::io::Read::read_to_end(&mut file, &mut bytes)?;
    validate_private_regular_file(path)?;
    Ok(bytes)
}

fn validate_private_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid("Invalid artifact storage directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(invalid("Invalid artifact storage owner or permissions"));
        }
    }
    Ok(())
}

fn validate_private_regular_file(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(invalid("Artifact body is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(invalid(
                "Invalid artifact body owner, permissions, or link count",
            ));
        }
    }
    Ok(())
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid_string(message: String) -> io::Error {
    invalid(&message)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::workflow::{
        ArtifactStatusV1, DecisionAlternativeV1, IdeaDecisionOutcomeV1, IdeaDecisionV1,
        ImplementationPlanV1, ProjectPrincipleV1, ProjectPrinciplesV1, RequirementV1,
        RoutineEligibilityV1, TaskDefinitionV1, TaskGraphV1, TaskRuntimeV1, TaskStateV1,
        UnresolvedDecisionV1, VerificationCriterionV1,
    };

    fn contract() -> ChangeContractV1 {
        ChangeContractV1::draft(
            "contract-a",
            RoutineEligibilityV1::local_component("polaris-core"),
            "save an immutable contract",
            "the graph only holds metadata",
            "the body is durable before the marker",
            vec!["store test passes".into()],
        )
        .unwrap()
    }

    #[test]
    fn same_body_is_idempotent_and_missing_or_tampered_body_fails() {
        let root = tempfile::tempdir().unwrap();
        let store = WorkflowArtifactStore::open(root.path(), "project-a").unwrap();
        let draft = contract();
        let reference = store.put_change_contract(&draft).unwrap();
        assert_eq!(store.put_change_contract(&draft).unwrap(), reference);

        let mut graph = WorkflowArtifactGraphV1::default();
        graph.insert_change_contract(&draft).unwrap();
        assert_eq!(
            store.load_change_contract(&graph, &reference).unwrap(),
            draft
        );

        let body = store.object_path_for_test(&reference);
        fs::remove_file(&body).unwrap();
        assert!(store.load_change_contract(&graph, &reference).is_err());

        store.put_change_contract(&draft).unwrap();
        fs::write(&body, b"{}").unwrap();
        assert!(store.load_change_contract(&graph, &reference).is_err());
        let approved = draft.revised(ArtifactStatusV1::Approved).unwrap();
        assert!(
            store
                .load_change_contract(&graph, &approved.reference())
                .is_err()
        );
    }

    #[test]
    fn loader_rejects_oversized_body_before_reading_it() {
        let root = tempfile::tempdir().unwrap();
        let store = WorkflowArtifactStore::open(root.path(), "project-oversized").unwrap();
        let draft = contract();
        let reference = store.put_change_contract(&draft).unwrap();
        let mut graph = WorkflowArtifactGraphV1::default();
        graph.insert_change_contract(&draft).unwrap();

        let body = store.object_path_for_test(&reference);
        fs::remove_file(&body).unwrap();
        let mut file = private_create_new(&body).unwrap();
        file.write_all(&vec![b'x'; MAX_WORKFLOW_ARTIFACT_BODY_BYTES + 1])
            .unwrap();
        drop(file);

        let error = store.load_change_contract(&graph, &reference).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    fn feature_plan_and_tasks() -> (FeatureSpecV1, ImplementationPlanV1, TaskGraphV1) {
        let feature = FeatureSpecV1::draft(
            "feature-store-1",
            "Artifact store",
            "Persist exact typed artifacts",
            vec![RequirementV1::stable("Bodies have exact graph references").unwrap()],
            vec![VerificationCriterionV1::stable("Typed bodies validate", true).unwrap()],
        )
        .unwrap();
        let plan = ImplementationPlanV1::draft(
            "plan-store-1",
            feature.reference(),
            "Publish feature, plan, then tasks",
            vec!["workflow_artifact_store.rs".into()],
        )
        .unwrap();
        let task_id = TaskDefinitionV1::stable_id("persist task graph").unwrap();
        let tasks = TaskGraphV1::draft(
            "tasks-store-1",
            feature.reference(),
            plan.reference(),
            vec![TaskDefinitionV1 {
                id: task_id.clone(),
                title: "Persist task graph".into(),
                scope: vec!["workflow_artifact_store.rs".into()],
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
        (feature, plan, tasks)
    }

    #[test]
    fn typed_artifacts_require_exact_graph_membership_and_intact_bodies() {
        let root = tempfile::tempdir().unwrap();
        let store = WorkflowArtifactStore::open(root.path(), "project-typed").unwrap();
        let (feature, plan, tasks) = feature_plan_and_tasks();

        let feature_ref = store.put_feature_spec(&feature).unwrap();
        assert_eq!(store.put_feature_spec(&feature).unwrap(), feature_ref);
        let plan_ref = store.put_implementation_plan(&plan).unwrap();
        assert_eq!(store.put_implementation_plan(&plan).unwrap(), plan_ref);
        let task_ref = store.put_task_graph(&tasks).unwrap();
        assert_eq!(store.put_task_graph(&tasks).unwrap(), task_ref);

        let mut graph = WorkflowArtifactGraphV1::default();
        graph.insert_feature_spec(&feature).unwrap();
        graph.insert_implementation_plan(&plan).unwrap();
        graph.insert_task_graph(&tasks).unwrap();
        assert_eq!(
            store.load_feature_spec(&graph, &feature_ref).unwrap(),
            feature
        );
        assert_eq!(
            store.load_implementation_plan(&graph, &plan_ref).unwrap(),
            plan
        );
        assert_eq!(store.load_task_graph(&graph, &task_ref).unwrap(), tasks);

        assert!(store.load_task_graph(&graph, &plan_ref).is_err());
        let unlisted = WorkflowArtifactGraphV1::default();
        assert!(store.load_feature_spec(&unlisted, &feature_ref).is_err());

        let body = store.object_path_for_test(&task_ref);
        fs::remove_file(&body).unwrap();
        assert!(store.load_task_graph(&graph, &task_ref).is_err());
        store.put_task_graph(&tasks).unwrap();
        fs::write(&body, b"{}").unwrap();
        assert!(store.load_task_graph(&graph, &task_ref).is_err());
    }

    #[test]
    fn principles_and_decisions_roundtrip_only_through_exact_graph_metadata() {
        let root = tempfile::tempdir().unwrap();
        let store = WorkflowArtifactStore::open(root.path(), "project-decisions").unwrap();
        let principles = ProjectPrinciplesV1::draft(
            "principles-store",
            vec![ProjectPrincipleV1 {
                id: "quality".into(),
                statement: "Keep durable bodies private.".into(),
                feature_ids: Vec::new(),
                task_ids: Vec::new(),
            }],
        )
        .unwrap();
        let decision = IdeaDecisionV1::draft(
            "decision-store",
            "Persist the initial decision.",
            vec!["Recovery needs it.".into()],
            Vec::new(),
            vec![DecisionAlternativeV1 {
                id: "chat-only".into(),
                description: "Keep chat as the only record.".into(),
            }],
            vec!["More state to validate.".into()],
            IdeaDecisionOutcomeV1::Clarify,
            vec![UnresolvedDecisionV1 {
                id: "cleanup".into(),
                question: "When can old bodies be collected?".into(),
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
                .load_project_principles(&graph, &principles_ref)
                .unwrap(),
            principles
        );
        assert_eq!(
            store.load_idea_decision(&graph, &decision_ref).unwrap(),
            decision
        );
        let path = store.object_path_for_test(&decision_ref);
        fs::write(path, b"{}").unwrap();
        assert!(store.load_idea_decision(&graph, &decision_ref).is_err());
    }
}

#[cfg(all(test, windows))]
mod windows_tests;
