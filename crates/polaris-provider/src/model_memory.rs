//! Planned additional local model allocations, never inferred from weight file size.
/// Known cloud clients do not load local weights. This is a transport contract,
/// not a connection/authentication check. Custom and loopback endpoints stay unknown.
pub(crate) fn cloud_allocation(base: &str, host: &str) -> Option<u64> {
    let url = reqwest::Url::parse(base).ok()?;
    (url.scheme() == "https"
        && url.host_str() == Some(host)
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none())
    .then_some(0)
}

/// Sum distinct provider instances only. Unknown allocations and overflow remain unknown.
pub fn planned_local_model_total<'a>(
    providers: impl IntoIterator<Item = &'a std::sync::Arc<dyn crate::Provider>>,
) -> Option<u64> {
    let mut seen: Vec<&std::sync::Arc<dyn crate::Provider>> = Vec::new();
    let mut total = 0u64;
    for provider in providers {
        if seen
            .iter()
            .any(|prior| std::sync::Arc::ptr_eq(prior, provider))
        {
            continue;
        }
        total = total.checked_add(provider.planned_local_model_bytes()?)?;
        seen.push(provider);
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_exact_cloud_origins_have_a_known_zero_local_model_plan() {
        assert_eq!(
            cloud_allocation("https://api.openai.com/v1", "api.openai.com"),
            Some(0)
        );
        for base in [
            "http://api.openai.com/v1",
            "https://api.openai.com:444/v1",
            "http://127.0.0.1:8080/v1",
            "https://api.openai.com.example/v1",
            "https://example.com/v1",
            "https://user@api.openai.com/v1",
        ] {
            assert_eq!(cloud_allocation(base, "api.openai.com"), None);
        }
    }
}
