//! Strict native launch argument grammar and legacy compatibility tests.
use super::*;
// Exact native fixture: whitespace-separated tokens, no shell quoting implied.
const OWNER_FIXTURE: &str = "--owner-launch-version 1 --store-root /private/tmp/polaris-store --project-id project --session-id session --store-device 1 --store-inode 2 --bootstrap-name owner-bootstrap.json --bootstrap-device 1 --bootstrap-inode 3 --bootstrap-sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef --confirmed-source-path /private/tmp/polaris-source --confirmed-source-device 1 --confirmed-source-inode 4 --confirmed-tier read_create_build --confirmed-policy-revision 5 --source-recovery-fd2";
fn args() -> Vec<OsString> {
    OWNER_FIXTURE.split_whitespace().map(Into::into).collect()
}
#[test]
fn native_owner_fixture_roundtrips_exact_values() {
    let LaunchArguments::OwnerV1(v) = parse_launch_args(&args()).unwrap() else {
        panic!("owner")
    };
    assert_eq!(
        v.store.store_root,
        PathBuf::from("/private/tmp/polaris-store")
    );
    assert_eq!(v.store.project_id.as_str(), "project");
    assert_eq!(v.store.session_id.as_str(), "session");
    assert_eq!(v.store_identity.device.get(), 1);
    assert_eq!(v.store_identity.inode.get(), 2);
    assert_eq!(v.bootstrap_name, "owner-bootstrap.json");
    assert_eq!(v.bootstrap_file.identity.device.get(), 1);
    assert_eq!(v.bootstrap_file.identity.inode.get(), 3);
    let hex = v
        .bootstrap_file
        .sha256
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    assert_eq!(
        hex,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );
    assert_eq!(
        v.confirmed_source_path,
        PathBuf::from("/private/tmp/polaris-source")
    );
    assert_eq!(v.confirmed_source_identity.device.get(), 1);
    assert_eq!(v.confirmed_source_identity.inode.get(), 4);
    assert_eq!(v.confirmed_tier, BootstrapTier::ReadCreateBuild);
    assert_eq!(v.confirmed_policy_revision.get(), 5);
    let reconstructed = format!(
        "--owner-launch-version 1 --store-root {} --project-id {} --session-id {} --store-device {} --store-inode {} --bootstrap-name {} --bootstrap-device {} --bootstrap-inode {} --bootstrap-sha256 {} --confirmed-source-path {} --confirmed-source-device {} --confirmed-source-inode {} --confirmed-tier read_create_build --confirmed-policy-revision {} --source-recovery-fd2",
        v.store.store_root.display(),
        v.store.project_id.as_str(),
        v.store.session_id.as_str(),
        v.store_identity.device.get(),
        v.store_identity.inode.get(),
        v.bootstrap_name,
        v.bootstrap_file.identity.device.get(),
        v.bootstrap_file.identity.inode.get(),
        hex,
        v.confirmed_source_path.display(),
        v.confirmed_source_identity.device.get(),
        v.confirmed_source_identity.inode.get(),
        v.confirmed_policy_revision.get()
    );
    assert_eq!(reconstructed, OWNER_FIXTURE);
}
#[test]
fn legacy_six_and_result_only_seven_are_distinct_from_owner() {
    let mut a: Vec<OsString> = [
        "--store-root",
        "relative-legacy",
        "--project-id",
        "p",
        "--session-id",
        "s",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    assert!(matches!(
        parse_launch_args(&a).unwrap(),
        LaunchArguments::Legacy {
            recovery_fd2: false,
            ..
        }
    ));
    a.push("--source-recovery-fd2".into());
    assert!(matches!(
        parse_launch_args(&a).unwrap(),
        LaunchArguments::Legacy {
            recovery_fd2: true,
            ..
        }
    ));
    a.push("2".into());
    assert!(parse_launch_args(&a).is_err());
}
#[test]
fn owner_rejects_missing_duplicate_reordered_unknown_and_invalid_values() {
    for index in 0..31 {
        let mut a = args();
        a.remove(index);
        assert!(parse_launch_args(&a).is_err());
    }
    for (index, value) in [
        (0, "--other"),
        (1, "2"),
        (3, "relative"),
        (3, "/a/../b"),
        (13, "../x"),
        (13, "x/y"),
        (13, "."),
        (9, "01"),
        (9, "18446744073709551616"),
        (19, "ABCDEF"),
        (21, "/a//b"),
        (27, "build"),
        (29, "-1"),
        (30, "--source-recovery-fd"),
    ] {
        let mut a = args();
        a[index] = value.into();
        assert!(parse_launch_args(&a).is_err(), "{index}");
    }
    let mut a = args();
    a.swap(2, 4);
    assert!(parse_launch_args(&a).is_err());
    let mut a = args();
    a[4] = "--store-root".into();
    assert!(parse_launch_args(&a).is_err());
    let mut a = args();
    a.extend([OsString::from("--helper"), OsString::from("/tmp/x")]);
    assert!(parse_launch_args(&a).is_err());
}
#[test]
fn owner_accepts_all_tiers_max_decimal_and_unicode_without_coercion() {
    for tier in [
        "read_only",
        "read_create",
        "read_create_build",
        "read_create_build_external",
    ] {
        let mut a = args();
        a[27] = tier.into();
        a[29] = u64::MAX.to_string().into();
        a[5] = "界".repeat(42).into();
        assert!(parse_launch_args(&a).is_ok());
    }
    use std::os::unix::ffi::OsStringExt;
    let mut a = args();
    a[13] = OsString::from_vec(vec![255]);
    assert!(parse_launch_args(&a).is_err());
}
