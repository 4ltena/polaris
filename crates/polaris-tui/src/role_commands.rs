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
        .ok_or("No saved conversation")?;
    let snapshot = persistence.snapshot().map_err(|e| e.to_string())?;
    let saved = snapshot.state.session_settings.as_ref().unwrap_or(initial);
    let request = request.trim().trim_start_matches('/');
    let request = request.strip_prefix("roles ").unwrap_or(request);
    if matches!(request, "" | "roles" | "current" | "list") {
        let mut lines = vec![Line::from(if saved.role_bindings_active {
            "Saved role bindings: enabled (effective from the next send; delegation to unbound roles is denied)"
        } else {
            "Saved role bindings: disabled (using existing provider delegation settings)"
        })];
        for (role, tier) in &saved.role_bindings {
            lines.push(Line::from(format!("{role} → {tier}")));
        }
        if saved.role_bindings.is_empty() {
            lines.push(Line::from("unbound"));
        }
        return Ok(lines);
    }
    if has_pending {
        return Err("Cannot change role bindings while input is queued".into());
    }
    session.check_persistence().map_err(|e| e.to_string())?;
    let mut bindings = saved.role_bindings.clone();
    let words: Vec<_> = request.split_whitespace().collect();
    match words.as_slice() {
        ["bind", role, tier] => {
            if !catalog.iter().any(|agent| agent.name == *role) {
                return Err("The role is not in the catalog".into());
            }
            if !Tier::ALL.iter().any(|known| known.name() == *tier) {
                return Err("Specify tier as dwarf|subgiant|giant|supergiant".into());
            }
            let actual = provider
                .resolve_tier(tier)
                .map_err(|e| e.to_string())?
                .ok_or("No provider configured for this tier")?;
            if actual.model().is_none() {
                return Err("Cannot verify the actual model for this tier".into());
            }
            bindings.insert(role.to_string(), tier.to_string());
        }
        ["unbind", role] | ["unbound", role] => {
            if bindings.remove(*role).is_none() {
                return Err("No binding for this role".into());
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
        .ok_or("No role binding storage target")?;
    validate_bindings(provider, candidate, catalog)?;
    let detail = format!(
        "{request}\nAfter change: {:?}\nUses only configured tier providers. Effective from the next send.\nDelegation to unbound roles is denied; no implicit fallback to the parent provider.",
        candidate.role_bindings
    );
    let lease = workspace
        .try_mutation()
        .map_err(|_| "An operation is active in the project")?;
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
        "Save role bindings",
        preview.session_id(),
        &detail,
    ) {
        owner.deny(&prepared).map_err(|e| e.to_string())?;
        return Ok(vec![Line::from("Role binding change cancelled")]);
    }
    let published = owner
        .execute(persistence, &prepared)
        .map_err(|e| e.to_string())?;
    Ok(vec![Line::from(format!(
        "D13 saved (conversation generation {}). Delegation to unbound roles is denied.",
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
            return Err(format!("Saved role is not in the catalog: {role}"));
        }
        let actual = provider
            .resolve_tier(tier)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("No provider configured for saved tier: {tier}"))?;
        if actual.model().is_none() {
            return Err(format!("Tier model unverified: {tier}"));
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
                .ok_or("The tier provider changed after confirmation")?,
        );
    }
    Ok(Arc::new(
        RoleProvider::new(provider, roles).with_tiers(tiers),
    ))
}

#[cfg(test)]
#[path = "role_commands/tests.rs"]
mod tests;
