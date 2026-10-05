//! Role bindings select existing fixed tier providers; they create no provider.
use crate::{account, approver::KeyReader};
use polaris_core::{
    management_effect::{ManagementEffectStoreV1, ManagementOperationIdV1},
    rewind_workspace::WorkspaceOwner,
    session::Session,
    session_settings::{SavedSessionSettingsV1, SessionSettingsCommandV1, SessionSettingsOwnerV1},
};
use polaris_provider::{Provider, role::RoleProvider};
use polaris_skills::{AgentType, Tier};
use ratatui::{Terminal, backend::Backend, text::Line};
use std::{cell::RefCell, collections::HashMap, sync::Arc};

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle<B: Backend, R: KeyReader>(
    request: &str,
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    session: &Session,
    initial: &SavedSessionSettingsV1,
    workspace: &WorkspaceOwner,
    effects: &ManagementEffectStoreV1,
    provider: &dyn Provider,
    catalog: &[AgentType],
    has_pending: bool,
) -> Result<Vec<Line<'static>>, String> {
    let persistence = session
        .persistence
        .as_ref()
        .ok_or("保存された会話がありません")?;
    let snapshot = persistence.snapshot().map_err(|e| e.to_string())?;
    let saved = snapshot.state.session_settings.as_ref().unwrap_or(initial);
    let request = request.trim().trim_start_matches('/');
    let request = request.strip_prefix("roles ").unwrap_or(request);
    if matches!(request, "" | "roles" | "current" | "list") {
        let mut lines = vec![Line::from(if saved.role_bindings_active {
            "保存role binding: 有効（次の送信から適用。未設定roleへの委譲は拒否します）"
        } else {
            "保存role binding: 無効（既存providerの委譲設定を使用します）"
        })];
        for (role, tier) in &saved.role_bindings {
            lines.push(Line::from(format!("{role} → {tier}")));
        }
        if saved.role_bindings.is_empty() {
            lines.push(Line::from("bindingなし"));
        }
        return Ok(lines);
    }
    if has_pending {
        return Err("送信待ちの入力があるためrole bindingを変更できません".into());
    }
    session.check_persistence().map_err(|e| e.to_string())?;
    let mut bindings = saved.role_bindings.clone();
    let words: Vec<_> = request.split_whitespace().collect();
    match words.as_slice() {
        ["bind", role, tier] => {
            if !catalog.iter().any(|agent| agent.name == *role) {
                return Err("catalogにないroleです".into());
            }
            if !Tier::ALL.iter().any(|known| known.name() == *tier) {
                return Err("tierはdwarf|subgiant|giant|supergiantで指定してください".into());
            }
            let actual = provider
                .resolve_tier(tier)
                .map_err(|e| e.to_string())?
                .ok_or("このtierのproviderは設定されていません")?;
            if actual.model().is_none() {
                return Err("このtierの実際のmodelを確認できません".into());
            }
            bindings.insert(role.to_string(), tier.to_string());
        }
        ["unbind", role] | ["unbound", role] => {
            if bindings.remove(*role).is_none() {
                return Err("このroleのbindingはありません".into());
            }
        }
        ["clear"] => bindings.clear(),
        _ => return Err("/roles bind <catalog-role> <tier> | unbind <role> | clear".into()),
    }
    let command = SessionSettingsCommandV1::RoleBindings {
        expected_revision: saved.revision,
        bindings,
    };
    let preview = SessionSettingsOwnerV1::preview(persistence, initial, command)
        .map_err(|e| e.to_string())?;
    let candidate = preview
        .candidate()
        .session_settings
        .as_ref()
        .ok_or("role binding保存対象がありません")?;
    validate_bindings(provider, candidate, catalog)?;
    let detail = format!(
        "{request}\n変更後: {:?}\n設定済みtier providerのみ使用。次の送信から適用。\n未設定roleへの委譲は拒否し、暗黙に親providerへ戻しません。",
        candidate.role_bindings
    );
    let lease = workspace
        .try_mutation()
        .map_err(|_| "projectで実行中の操作があります")?;
    workspace
        .require_quiescent_with_lease(&lease)
        .map_err(|e| e.to_string())?;
    let owner = SessionSettingsOwnerV1 {
        effects,
        lease: &lease,
    };
    let prepared = owner
        .prepare(
            persistence,
            ManagementOperationIdV1::random().map_err(|e| e.to_string())?,
            &preview,
        )
        .map_err(|e| e.to_string())?;
    if !account::confirm(
        terminal,
        reader,
        "role bindingを保存",
        preview.session_id(),
        &detail,
    ) {
        owner.deny(&prepared).map_err(|e| e.to_string())?;
        return Ok(vec![Line::from("role bindingの変更を取り消しました")]);
    }
    let published = owner
        .execute(persistence, &prepared)
        .map_err(|e| e.to_string())?;
    Ok(vec![Line::from(format!(
        "D13を保存しました（会話世代 {}）。未設定roleへの委譲は拒否します。",
        published.state.generation
    ))])
}

fn validate_bindings(
    provider: &dyn Provider,
    settings: &SavedSessionSettingsV1,
    catalog: &[AgentType],
) -> Result<(), String> {
    settings.validate().map_err(|e| e.to_string())?;
    if !settings.role_bindings_active {
        return Ok(());
    }
    for (role, tier) in &settings.role_bindings {
        if !catalog.iter().any(|agent| agent.name == *role) {
            return Err(format!("catalogにない保存role: {role}"));
        }
        let actual = provider
            .resolve_tier(tier)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("保存tierのproviderが未設定: {tier}"))?;
        if actual.model().is_none() {
            return Err(format!("tier model未確認: {tier}"));
        }
    }
    Ok(())
}

pub(crate) fn activation(
    provider: Arc<dyn Provider>,
    settings: &SavedSessionSettingsV1,
    catalog: &[AgentType],
) -> Result<Arc<dyn Provider>, String> {
    validate_bindings(provider.as_ref(), settings, catalog)?;
    if !settings.role_bindings_active {
        return Ok(provider);
    }
    let mut tiers = HashMap::new();
    for tier in Tier::ALL {
        if let Some(actual) = provider
            .resolve_tier(tier.name())
            .map_err(|e| e.to_string())?
        {
            tiers.insert(tier.name().to_owned(), actual);
        }
    }
    let mut roles = HashMap::new();
    for (role, tier) in &settings.role_bindings {
        roles.insert(
            role.clone(),
            tiers
                .get(tier)
                .cloned()
                .ok_or("確認後にtier providerが変わりました")?,
        );
    }
    Ok(Arc::new(
        RoleProvider::new(provider, roles).with_tiers(tiers),
    ))
}

#[cfg(test)]
#[path = "role_commands/tests.rs"]
mod tests;
