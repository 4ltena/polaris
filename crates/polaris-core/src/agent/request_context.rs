//! Shared assembly for the sections appended to a request system prompt.

/// Appends request-specific sections in their wire order.
///
/// The base system comes from either the existing child prompt or the root's
/// fresh AGENTS read. MCP metadata is intentionally added by the caller after
/// this composition, because it is selected with the request's tool schemas.
pub(super) fn compose(
    base: &str,
    workflow: Option<&str>,
    catalog: Option<&str>,
    filemap: Option<&str>,
    decisions: Option<&str>,
    analysis: Option<&str>,
) -> String {
    let mut system = base.to_owned();
    if let Some(workflow) = workflow {
        system.push('\n');
        system.push_str(workflow);
    }
    if let Some(catalog) = catalog
        && !catalog.is_empty()
    {
        system.push('\n');
        system.push_str(catalog);
    }
    // The routed files come last, so the sections that answer to their own
    // ceilings keep the order they had before this existed.
    if let Some(filemap) = filemap {
        system.push('\n');
        system.push_str(crate::filemap::HEADING);
        system.push('\n');
        system.push_str(filemap);
    }
    if let Some(decisions) = decisions {
        system.push('\n');
        system.push_str(crate::decisions::HEADING);
        system.push('\n');
        system.push_str(decisions);
    }
    if let Some(analysis) = analysis {
        system.push('\n');
        system.push_str(analysis);
    }
    system
}
