//! Holds a private executable package for real CLI integration-test children.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Package {
    bundle: tempfile::TempDir,
    temporary: tempfile::TempDir,
}

thread_local! {
    // Cargo can hardlink its output again whenever it starts a test target.
    // Keep our separate copy until this thread has finished with its children.
    static PACKAGE: Package = {
        let source = Path::new(env!("CARGO_BIN_EXE_polaris"));
        let metadata = fs::symlink_metadata(source).expect("built CLI metadata");
        assert!(metadata.is_file() && metadata.len() > 0);
        let directory = tempfile::Builder::new()
            .prefix("polaris-cli-package-")
            .tempdir()
            .expect("private CLI package directory");
        let temporary = tempfile::Builder::new()
            .prefix("polaris-cli-temporary-")
            .tempdir()
            .expect("separate private CLI temporary directory");
        let home = directory.path().join("home");
        fs::create_dir(&home).expect("private default CLI home");
        let destination = directory.path().join(source.file_name().expect("CLI filename"));
        assert_eq!(
            fs::copy(source, &destination).expect("copy the built CLI"),
            metadata.len(),
            "the CLI package must contain the complete built executable"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
            let copied = fs::symlink_metadata(&destination).expect("packaged CLI metadata");
            assert_eq!(copied.nlink(), 1);
            assert_eq!(copied.uid(), directory.path().metadata().unwrap().uid());
            assert_eq!(copied.mode() & 0o7022, 0);
            assert_ne!(copied.mode() & 0o111, 0);
        }
        Package { bundle: directory, temporary }
    };
}

pub fn product_binary() -> PathBuf {
    PACKAGE.with(|package| {
        package.bundle.path().join(
            Path::new(env!("CARGO_BIN_EXE_polaris"))
                .file_name()
                .expect("CLI filename"),
        )
    })
}

pub fn command() -> Command {
    let binary = product_binary();
    let home = binary.parent().expect("CLI package directory").join("home");
    let temporary = PACKAGE.with(|package| package.temporary.path().to_owned());
    let mut command = Command::new(binary);
    // Each fixture supplies its HOME or an explicit data root. Never inherit
    // a developer's ambient override for those synthetic homes.
    command.env_remove("POLARIS_DATA_DIR");
    command.env("HOME", home);
    command.env("TMPDIR", temporary);
    command
}
