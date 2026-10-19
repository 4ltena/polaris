//! Read-only inspection of a disposable, unpublished macOS package.
#[cfg(target_os = "macos")]
fn main() {
    let arguments: Vec<_> = std::env::args_os().collect();
    if arguments.len() != 2 {
        eprintln!("owned_entry_package_arguments");
        std::process::exit(64);
    }
    match polaris_mcp::entry_gate::ProductionEntryManifestV2::from_macos_package(
        std::path::Path::new(&arguments[1]),
    ) {
        Ok(manifest) => println!(
            "{}",
            serde_json::json!({
                "status": "passed",
                "protocol_version": manifest.protocol_version,
                "entry_count": manifest.entries.len(),
                "entry_kinds": manifest.entries.iter().map(|entry| entry.kind).collect::<Vec<_>>(),
                "runtime_copy_source_count": manifest.runtime_copy_source_hashes.len(),
            })
        ),
        Err(_) => {
            eprintln!("owned_entry_package_refused");
            std::process::exit(1);
        }
    }
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("owned_entry_package_unavailable");
    std::process::exit(78);
}
