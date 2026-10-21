//! 保存会話の設定とプロセス所有サービスを実行中のセッションへ適用する。

use std::path::Path;
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::time::Instant;

use polaris_core::session::Session;
#[cfg(unix)]
use polaris_core::session_settings::{SavedSessionSettingsV1, ToolMemoryModeV1};
use polaris_core::tool_memory::RetentionMode;
use polaris_provider::Provider;

#[cfg(unix)]
use crate::input;
use crate::{memory, persist, tool_memory};

#[cfg(unix)]
pub(super) struct SavedActivationRead {
    settings: SavedSessionSettingsV1,
    draft: Option<String>,
}

#[cfg(unix)]
pub(super) fn read_saved_activation(
    session: &Session,
    initial_settings: &SavedSessionSettingsV1,
    current_model: &str,
    switched: bool,
) -> Result<Option<SavedActivationRead>, String> {
    let Some(saved) = &session.persistence else {
        return Ok(None);
    };
    let snapshot = saved
        .snapshot()
        .map_err(|error| format!("会話設定を復元できません: {error}"))?;
    let settings = snapshot.state.session_settings.unwrap_or_else(|| {
        let mut initial = initial_settings.clone();
        initial.model = current_model.to_owned();
        initial
    });
    let draft = switched.then_some(snapshot.state.draft.text);
    Ok(Some(SavedActivationRead { settings, draft }))
}

#[cfg(unix)]
pub(super) fn apply_saved_binding(
    activation: &SavedActivationRead,
    effective_settings: &mut SavedSessionSettingsV1,
    provider_name: &str,
    provider: &dyn Provider,
    model_name: &mut String,
) -> Result<(), String> {
    effective_settings.clone_from(&activation.settings);
    if effective_settings.provider != provider_name {
        return Err("保存providerが起動時providerと異なります。指定して再起動してください".into());
    }
    if *model_name != effective_settings.model {
        if provider.model().is_none() {
            return Err("保存modelをこのproviderへ適用できません".into());
        }
        provider.set_model(&effective_settings.model);
        if provider.model().as_deref() != Some(effective_settings.model.as_str()) {
            return Err("保存modelの適用を確認できません".into());
        }
        model_name.clone_from(&effective_settings.model);
    }
    Ok(())
}

#[cfg(unix)]
pub(super) struct SavedDraftTarget<'a> {
    pub(super) input_buffer: &'a mut String,
    pub(super) input_cursor: &'a mut usize,
    pub(super) input_history: &'a mut input::History,
    pub(super) saved_text: &'a mut String,
    pub(super) candidate: &'a mut String,
    #[cfg(target_os = "linux")]
    pub(super) changed_at: &'a mut Instant,
}

#[cfg(unix)]
pub(super) fn apply_saved_draft(activation: SavedActivationRead, target: SavedDraftTarget<'_>) {
    let Some(draft) = activation.draft else {
        return;
    };
    *target.input_buffer = draft;
    *target.input_cursor = target.input_buffer.len();
    *target.input_history = input::History::new();
    target.saved_text.clone_from(target.input_buffer);
    target.candidate.clone_from(target.input_buffer);
    #[cfg(target_os = "linux")]
    {
        *target.changed_at = Instant::now();
    }
}

pub(super) struct ProcessServices<'a> {
    pub(super) decider: &'a Option<Arc<dyn polaris_provider::decision::Decider>>,
    pub(super) tier_decider: &'a Option<Arc<dyn polaris_core::tier::TierDecider>>,
    pub(super) light_model: &'a Option<Arc<dyn Provider>>,
    pub(super) approval_auto: polaris_core::approval::AutoApproval,
    pub(super) setting: &'a Arc<polaris_core::recipe::tool::SettingTool>,
    pub(super) computer: &'a Option<Arc<polaris_core::computer::ComputerTool>>,
}

pub(super) fn attach_process_services(session: &mut Session, services: ProcessServices<'_>) {
    session.decider = services.decider.clone();
    session.tier_decider = services.tier_decider.clone();
    session.light_model = services.light_model.clone();
    session.approval_auto = services.approval_auto;
    session.setting = Some(services.setting.clone());
    session.computer = services.computer.clone();
}

pub(super) struct MemoryActivation {
    tool_mode: Option<RetentionMode>,
    embedding: Option<(String, String)>,
    compact_threshold: Option<usize>,
    remember: bool,
}

impl MemoryActivation {
    pub(super) fn unverified_model_allocation(&self) -> bool {
        self.tool_mode.is_some() && self.embedding.is_some()
    }

    #[cfg(unix)]
    pub(super) fn from_saved(settings: &SavedSessionSettingsV1) -> Self {
        let tool_mode = match settings.tool_memory {
            ToolMemoryModeV1::Off => None,
            ToolMemoryModeV1::History => Some(RetentionMode::History),
            ToolMemoryModeV1::Retrieval => Some(RetentionMode::Retrieval),
        };
        Self {
            tool_mode,
            embedding: settings
                .tool_memory_embedding
                .as_ref()
                .map(|pair| (pair.url.clone(), pair.model.clone())),
            compact_threshold: settings.compact_threshold,
            remember: settings.remember,
        }
    }

    #[cfg(not(unix))]
    pub(super) fn from_runtime(
        tool_mode: Option<RetentionMode>,
        embedding: Option<(String, String)>,
        compact_threshold: Option<usize>,
        remember: bool,
    ) -> Self {
        Self {
            tool_mode,
            embedding,
            compact_threshold,
            remember,
        }
    }

    #[cfg(unix)]
    pub(super) fn tool_memory_enabled(&self) -> bool {
        self.tool_mode.is_some()
    }

    #[cfg(unix)]
    pub(super) fn embedding(&self) -> Option<&(String, String)> {
        self.embedding.as_ref()
    }
}

pub(super) fn apply_tool_memory(
    session: &mut Session,
    project: &Path,
    state_dir: &Path,
    session_path: &Path,
    active_path: Option<&Path>,
    settings: &MemoryActivation,
) -> Result<bool, String> {
    if active_path != Some(session_path)
        || (settings.tool_mode.is_some()
            && session.tool_memory.is_none()
            && memory::saved_project_matches(session, project, session_path))
    {
        configure_saved_tool_memory(
            session,
            project,
            state_dir,
            session_path,
            settings.tool_mode,
            settings.embedding.clone(),
        )
        .map_err(|error| format!("ツール記憶を準備できません: {error}"))?;
        return Ok(true);
    }
    Ok(false)
}

pub(super) fn apply_compaction_and_memory(
    session: &mut Session,
    project: &Path,
    state_dir: &Path,
    session_path: &Path,
    settings: &MemoryActivation,
) -> Result<(), String> {
    session.compaction_threshold = settings.compact_threshold;
    if settings.remember {
        memory::configure_saved_memory(session, project, state_dir, session_path)
            .map_err(|error| format!("履歴保存を準備できません: {error}"))?;
    } else if session.persistence.is_some() {
        session.before_compact = None;
    } else if session.persistence.is_none()
        && session.before_compact.is_none()
        && let Some(id) = session_path.file_stem().and_then(|value| value.to_str())
    {
        // Without this the legacy log is rewritten over the summary and
        // the compacted prefix is gone from memory and disk at once.
        memory::configure_archive(session, project, state_dir, id)
            .map_err(|error| format!("圧縮前の履歴を退避できません: {error}"))?;
    }
    Ok(())
}

// Ancestors are read-only and must form an explicit, bounded same-project chain.
pub(super) const MAX_TOOL_MEMORY_ORIGINS: usize = 64;

pub(super) fn saved_tool_memory_namespace(
    project: &Path,
    path: &Path,
) -> std::io::Result<(String, Vec<String>)> {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
        .ok_or_else(|| std::io::Error::other("invalid session id"))?
        .to_owned();
    let project = polaris_core::project::resolve_root(project);
    let mut seen = std::collections::HashSet::from([id.clone()]);
    let mut origins = Vec::new();
    let mut cursor = path.to_path_buf();
    while let Some(origin) = persist::read_tool_memory_origin(&cursor.with_extension("meta.json"))?
    {
        if origins.len() >= MAX_TOOL_MEMORY_ORIGINS || !seen.insert(origin.clone()) {
            return Err(std::io::Error::other(
                "記憶の系譜が循環しているか上限を超えています",
            ));
        }
        let source = path.with_file_name(format!("{origin}.jsonl"));
        let source_meta = persist::read_meta(&source.with_extension("meta.json"))
            .ok_or_else(|| std::io::Error::other("記憶の元セッションの出自を確認できません"))?;
        if !source.is_file()
            || !Path::new(&source_meta.cwd).is_absolute()
            || polaris_core::project::resolve_root(Path::new(&source_meta.cwd)) != project
        {
            return Err(std::io::Error::other(
                "記憶の元セッションが存在しないか同一プロジェクトではありません",
            ));
        }
        origins.push(origin);
        cursor = source;
    }
    Ok((id, origins))
}

pub(super) fn configure_saved_tool_memory(
    session: &mut Session,
    project: &Path,
    state_dir: &Path,
    session_path: &Path,
    mode: Option<RetentionMode>,
    embedding: Option<(String, String)>,
) -> std::io::Result<()> {
    session.tool_memory = None;
    if let Some(mode) = mode
        && memory::saved_project_matches(session, project, session_path)
    {
        if let Some(persistence) = session.persistence.as_ref() {
            let snapshot = persistence.snapshot()?;
            let mut origins = snapshot
                .state
                .ancestors
                .iter()
                .map(|ancestor| ancestor.session_id.clone())
                .collect::<Vec<_>>();
            origins.sort();
            origins.dedup();
            if origins.len() > MAX_TOOL_MEMORY_ORIGINS {
                return Err(std::io::Error::other("記憶の系譜が上限を超えています"));
            }
            if let Some(branch) = persistence.store.rewind_branch_metadata()? {
                if !origins.contains(&branch.origin_session_id) {
                    return Err(std::io::Error::other(
                        "rewind分岐の記憶出自が保存された系譜と一致しません",
                    ));
                }
                tool_memory::configure_tool_memory_with_origin_allowlist(
                    session,
                    project,
                    state_dir,
                    &snapshot.state.session_id,
                    &origins,
                    &branch.visible_memory_raw_object_ids,
                    mode,
                    embedding,
                )?;
            } else {
                tool_memory::configure_tool_memory_with_origins(
                    session,
                    project,
                    state_dir,
                    &snapshot.state.session_id,
                    &origins,
                    mode,
                    embedding,
                )?;
            }
            return Ok(());
        }
        let (id, origins) = saved_tool_memory_namespace(project, session_path)?;
        tool_memory::configure_tool_memory_with_origins(
            session, project, state_dir, &id, &origins, mode, embedding,
        )?;
    }
    Ok(())
}
