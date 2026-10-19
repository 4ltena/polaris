//! Owned macOS DP Keychain validation. Never queries existing user items.
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("owned_dp_probe_unavailable");
    std::process::exit(78);
}

#[cfg(target_os = "macos")]
mod probe {
    use core_foundation::{
        base::{CFType, TCFType},
        boolean::CFBoolean,
        data::CFData,
        dictionary::CFDictionary,
        number::CFNumber,
        string::{CFString, CFStringRef},
    };
    use polaris_auth::git_credentials::{
        CanonicalHttpsRemoteV1, GitCredentialErrorV1, GitCredentialOwnerV1, OptionalAttributeHashV1,
    };
    use ring::{
        digest,
        rand::{SecureRandom, SystemRandom},
    };
    use security_framework_sys::{
        item::*,
        keychain_item::{SecItemAdd, SecItemCopyMatching, SecItemDelete, SecItemUpdate},
    };
    use std::{os::unix::fs::PermissionsExt, path::PathBuf};
    use zeroize::Zeroizing;
    unsafe extern "C" {
        static kSecValuePersistentRef: CFStringRef;
        static kSecAttrAccessible: CFStringRef;
        static kSecAttrProtocolHTTPS: CFStringRef;
        static kSecAttrProtocolHTTP: CFStringRef;
        static kSecAttrAuthenticationTypeDefault: CFStringRef;
        static kSecAttrAuthenticationTypeHTTPBasic: CFStringRef;
        static kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly: CFStringRef;
    }
    type Failure = &'static str;
    const ACCESS_GROUP: &str = "W5C5A2892Z.jp.4ltena.polaris.c10p0";
    fn key(value: CFStringRef) -> CFString {
        unsafe { CFString::wrap_under_get_rule(value) }
    }
    fn value(value: CFStringRef) -> CFType {
        key(value).into_CFType()
    }
    fn check(ok: bool, category: Failure) -> Result<(), Failure> {
        if ok { Ok(()) } else { Err(category) }
    }
    struct OwnedItems {
        host: String,
        references: Vec<CFData>,
    }
    impl OwnedItems {
        fn base(&self) -> Vec<(CFString, CFType)> {
            vec![
                (
                    key(unsafe { kSecClass }),
                    value(unsafe { kSecClassInternetPassword }),
                ),
                (
                    key(unsafe { kSecUseDataProtectionKeychain }),
                    CFBoolean::true_value().into_CFType(),
                ),
                (
                    key(unsafe { kSecAttrAccessGroup }),
                    CFString::new(ACCESS_GROUP).into_CFType(),
                ),
                (
                    key(unsafe { kSecAttrSynchronizable }),
                    CFBoolean::false_value().into_CFType(),
                ),
                (
                    key(unsafe { kSecAttrServer }),
                    CFString::new(&self.host).into_CFType(),
                ),
                (
                    key(unsafe { kSecAttrAccount }),
                    CFString::new("polaris-owned-dp").into_CFType(),
                ),
                (
                    key(unsafe { kSecAttrPath }),
                    CFString::new("/owned/p1").into_CFType(),
                ),
                (
                    key(unsafe { kSecAttrPort }),
                    CFNumber::from(443_i32).into_CFType(),
                ),
                (
                    key(unsafe { kSecAttrProtocol }),
                    value(unsafe { kSecAttrProtocolHTTPS }),
                ),
            ]
        }
        fn add(&mut self, basic: bool, domain: Option<&str>) -> Result<CFData, Failure> {
            let mut pairs = self.base();
            pairs.push((
                key(unsafe { kSecAttrAuthenticationType }),
                value(unsafe {
                    if basic {
                        kSecAttrAuthenticationTypeHTTPBasic
                    } else {
                        kSecAttrAuthenticationTypeDefault
                    }
                }),
            ));
            if let Some(domain) = domain {
                pairs.push((
                    key(unsafe { kSecAttrSecurityDomain }),
                    CFString::new(domain).into_CFType(),
                ));
            }
            let cleanup = CFDictionary::from_CFType_pairs(&pairs);
            let secret = secret()?;
            pairs.extend([
                (
                    key(unsafe { kSecAttrAccessible }),
                    value(unsafe { kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly }),
                ),
                (
                    key(unsafe { kSecValueData }),
                    CFData::from_buffer(&secret).into_CFType(),
                ),
                (
                    key(unsafe { kSecReturnPersistentRef }),
                    CFBoolean::true_value().into_CFType(),
                ),
            ]);
            let query = CFDictionary::from_CFType_pairs(&pairs);
            let mut result = std::ptr::null();
            if unsafe { SecItemAdd(query.as_concrete_TypeRef(), &mut result) } != 0 {
                return Err("owned_add");
            }
            if result.is_null() {
                check(
                    unsafe { SecItemDelete(cleanup.as_concrete_TypeRef()) } == 0,
                    "owned_cleanup_reference_failure",
                )?;
                return Err("owned_reference_type");
            }
            let result = unsafe { CFType::wrap_under_create_rule(result) };
            let Some(reference) = result.downcast::<CFData>() else {
                check(
                    unsafe { SecItemDelete(cleanup.as_concrete_TypeRef()) } == 0,
                    "owned_cleanup_reference_failure",
                )?;
                return Err("owned_reference_type");
            };
            self.references.push(reference.clone());
            Ok(reference)
        }
        fn query_reference(reference: &CFData) -> CFDictionary<CFString, CFType> {
            CFDictionary::from_CFType_pairs(&[
                (
                    key(unsafe { kSecClass }),
                    value(unsafe { kSecClassInternetPassword }),
                ),
                (
                    key(unsafe { kSecUseDataProtectionKeychain }),
                    CFBoolean::true_value().into_CFType(),
                ),
                (
                    key(unsafe { kSecValuePersistentRef }),
                    reference.clone().into_CFType(),
                ),
            ])
        }
        fn optional_attribute(
            row: &CFDictionary<CFString, CFType>,
            field: CFStringRef,
        ) -> Result<OptionalAttributeHashV1, Failure> {
            let Some(value) = row.find(key(field)) else {
                return Ok(OptionalAttributeHashV1::Absent);
            };
            let value = value
                .downcast::<CFString>()
                .ok_or("owned_attribute_type")?
                .to_string();
            Ok(OptionalAttributeHashV1::Present {
                sha256: digest::digest(&digest::SHA256, value.as_bytes())
                    .as_ref()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
            })
        }
        fn owned_snapshot(
            &self,
            reference: &CFData,
        ) -> Result<CFDictionary<CFString, CFType>, Failure> {
            let pairs = vec![
                (
                    key(unsafe { kSecClass }),
                    value(unsafe { kSecClassInternetPassword }),
                ),
                (
                    key(unsafe { kSecUseDataProtectionKeychain }),
                    CFBoolean::true_value().into_CFType(),
                ),
                (
                    key(unsafe { kSecValuePersistentRef }),
                    reference.clone().into_CFType(),
                ),
                (
                    key(unsafe { kSecReturnAttributes }),
                    CFBoolean::true_value().into_CFType(),
                ),
                (
                    key(unsafe { kSecReturnPersistentRef }),
                    CFBoolean::true_value().into_CFType(),
                ),
            ];
            let query = CFDictionary::from_CFType_pairs(&pairs);
            let mut result = std::ptr::null();
            let status = unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), &mut result) };
            if status != 0 || result.is_null() {
                println!(
                    "{}",
                    serde_json::json!({
                        "owned_attribute_diagnostic":true,
                        "direct_reference_query_status":status,
                        "returned_null":result.is_null()
                    })
                );
                return Err("owned_attribute_snapshot");
            }
            let result = unsafe { CFType::wrap_under_create_rule(result) };
            let raw = result
                .downcast::<CFDictionary>()
                .ok_or("owned_attribute_dictionary")?;
            let row = unsafe {
                CFDictionary::<CFString, CFType>::wrap_under_get_rule(raw.as_concrete_TypeRef())
            };
            let string = |field| -> Result<String, Failure> {
                row.find(key(field))
                    .ok_or("owned_snapshot_field")?
                    .downcast::<CFString>()
                    .ok_or("owned_snapshot_type")
                    .map(|value| value.to_string())
            };
            let group = string(unsafe { kSecAttrAccessGroup })?;
            let host = string(unsafe { kSecAttrServer })?;
            let account = string(unsafe { kSecAttrAccount })?;
            let returned_reference = row
                .find(key(unsafe { kSecValuePersistentRef }))
                .ok_or("owned_snapshot_reference")?
                .downcast::<CFData>()
                .ok_or("owned_snapshot_reference_type")?;
            check(
                group == ACCESS_GROUP
                    && (host == self.host || host == format!("drift-{}", self.host))
                    && matches!(account.as_str(), "polaris-owned-dp" | "owned-other")
                    && returned_reference.bytes() == reference.bytes(),
                "owned_snapshot_boundary",
            )?;
            Ok(row)
        }
        fn actual_optional_attributes(
            &self,
            reference: &CFData,
        ) -> Result<(OptionalAttributeHashV1, OptionalAttributeHashV1), Failure> {
            let row = self.owned_snapshot(reference)?;
            Ok((
                Self::optional_attribute(&row, unsafe { kSecAttrSecurityDomain })?,
                Self::optional_attribute(&row, unsafe { kSecAttrAccessGroup })?,
            ))
        }
        fn diagnostic_attributes(&self, reference: &CFData) {
            let pairs = [
                (
                    key(unsafe { kSecClass }),
                    value(unsafe { kSecClassInternetPassword }),
                ),
                (
                    key(unsafe { kSecUseDataProtectionKeychain }),
                    CFBoolean::true_value().into_CFType(),
                ),
                (
                    key(unsafe { kSecValuePersistentRef }),
                    reference.clone().into_CFType(),
                ),
                (
                    key(unsafe { kSecReturnAttributes }),
                    CFBoolean::true_value().into_CFType(),
                ),
                (
                    key(unsafe { kSecReturnPersistentRef }),
                    CFBoolean::true_value().into_CFType(),
                ),
            ];
            let query = CFDictionary::from_CFType_pairs(&pairs);
            let mut result = std::ptr::null();
            let status = unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), &mut result) };
            if status != 0 || result.is_null() {
                println!(
                    "{}",
                    serde_json::json!({"owned_attribute_diagnostic":true,"query_status":status})
                );
                return;
            }
            let result = unsafe { CFType::wrap_under_create_rule(result) };
            let Some(raw) = result.downcast::<CFDictionary>() else {
                println!(
                    "{}",
                    serde_json::json!({"owned_attribute_diagnostic":true,"result_type":result.type_of()})
                );
                return;
            };
            let row = unsafe {
                CFDictionary::<CFString, CFType>::wrap_under_get_rule(raw.as_concrete_TypeRef())
            };
            let (keys, values) = row.get_keys_and_values();
            let types = keys
                .into_iter()
                .zip(values)
                .map(|(name, value)| {
                    let name =
                        unsafe { CFString::wrap_under_get_rule(name as CFStringRef) }.to_string();
                    let value = unsafe { CFType::wrap_under_get_rule(value) };
                    (name, value.type_of())
                })
                .collect::<Vec<_>>();
            println!(
                "{}",
                serde_json::json!({"owned_attribute_diagnostic":true,"attribute_types":types})
            );
        }
        fn update(
            &self,
            reference: &CFData,
            field: CFStringRef,
            next: CFType,
        ) -> Result<(), Failure> {
            self.owned_snapshot(reference)?;
            let query = Self::query_reference(reference);
            let changes = CFDictionary::from_CFType_pairs(&[(key(field), next)]);
            check(
                unsafe {
                    SecItemUpdate(query.as_concrete_TypeRef(), changes.as_concrete_TypeRef())
                } == 0,
                "owned_update",
            )
        }
        fn delete(&mut self, reference: &CFData) -> Result<(), Failure> {
            self.owned_snapshot(reference)?;
            let query = Self::query_reference(reference);
            check(
                unsafe { SecItemDelete(query.as_concrete_TypeRef()) } == 0,
                "owned_delete",
            )?;
            self.references
                .retain(|item| item.bytes() != reference.bytes());
            Ok(())
        }
        fn cleanup(&mut self) -> Result<(), Failure> {
            while let Some(reference) = self.references.last().cloned() {
                self.delete(&reference)?;
            }
            Ok(())
        }
    }
    impl Drop for OwnedItems {
        fn drop(&mut self) {
            for reference in &self.references {
                if self.owned_snapshot(reference).is_err() {
                    continue;
                }
                let query = Self::query_reference(reference);
                unsafe {
                    SecItemDelete(query.as_concrete_TypeRef());
                }
            }
        }
    }
    fn secret() -> Result<Zeroizing<Vec<u8>>, Failure> {
        let mut raw = Zeroizing::new([0_u8; 32]);
        SystemRandom::new()
            .fill(raw.as_mut())
            .map_err(|_| "owned_random")?;
        Ok(Zeroizing::new(
            raw.iter()
                .flat_map(|byte| format!("{byte:02x}").into_bytes())
                .collect(),
        ))
    }
    pub fn run() -> Result<(), Failure> {
        let args: Vec<_> = std::env::args_os().collect();
        check(args.len() == 3, "owned_arguments")?;
        let git = PathBuf::from(&args[1]);
        let askpass = PathBuf::from(&args[2]);
        let home = tempfile::tempdir().map_err(|_| "owned_home")?;
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "owned_home_mode")?;
        let parent = home.path().join(".polaris");
        std::fs::create_dir(&parent).map_err(|_| "owned_parent")?;
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "owned_parent_mode")?;
        let owner = GitCredentialOwnerV1::new(home.path(), &git, &askpass)
            .map_err(|_| "owned_binary_owner")?;
        let nonce = secret()?;
        let host = format!(
            "polaris-p1-{}.invalid",
            std::str::from_utf8(&nonce[..24]).map_err(|_| "owned_nonce")?
        );
        let mut items = OwnedItems {
            host: host.clone(),
            references: vec![],
        };
        let result = (|| {
            let remote = CanonicalHttpsRemoteV1::parse(&format!("https://{host}:443/owned/p1"))
                .map_err(|_| "owned_remote")?;
            let account = "polaris-owned-dp";
            check(
                owner
                    .candidate_choices(&remote, account)
                    .map_err(|_| "candidate_zero")?
                    .is_empty(),
                "candidate_zero",
            )?;
            check(
                matches!(
                    owner.select(&remote, account, 1, None),
                    Err(GitCredentialErrorV1::Missing)
                ),
                "select_zero",
            )?;
            let reference = items.add(false, None)?;
            check(
                owner
                    .candidate_choices(&remote, account)
                    .map_err(|_| {
                        items.diagnostic_attributes(&reference);
                        "candidate_one"
                    })?
                    .len()
                    == 1,
                "candidate_one",
            )?;
            let (actual_domain, actual_group) = items.actual_optional_attributes(&reference)?;
            let (binding, material) =
                owner
                    .select(&remote, account, 1, None)
                    .map_err(|error| match error {
                        GitCredentialErrorV1::Protection => "select_one_protection",
                        GitCredentialErrorV1::BindingChanged => "select_one_binding_changed",
                        GitCredentialErrorV1::Keychain => "select_one_keychain",
                        GitCredentialErrorV1::Io(_) => "select_one_io",
                        GitCredentialErrorV1::InvalidBinding => "select_one_invalid_binding",
                        GitCredentialErrorV1::Missing => "select_one_missing",
                        GitCredentialErrorV1::Ambiguous => "select_one_ambiguous",
                        GitCredentialErrorV1::InvalidRemote => "select_one_invalid_remote",
                        GitCredentialErrorV1::UnavailablePlatform => "select_one_unavailable",
                    })?;
            check(
                binding.security_domain == actual_domain && binding.access_group == actual_group,
                "attribute_snapshot_binding",
            )?;
            let no_domain_returned_present =
                matches!(actual_domain, OptionalAttributeHashV1::Present { .. });
            check(!material.password().is_empty(), "material_present")?;
            drop(material);
            owner
                .revalidate_binding(&binding)
                .map_err(|_| "binding_revalidation")?;
            let basic = items.add(true, None)?;
            let choices = owner
                .candidate_choices(&remote, account)
                .map_err(|_| "candidate_two")?;
            check(choices.len() == 2, "candidate_two")?;
            check(
                matches!(
                    owner.select(&remote, account, 1, None),
                    Err(GitCredentialErrorV1::Ambiguous)
                ),
                "select_ambiguous",
            )?;
            let prefix = &binding.credential_item_id[..16];
            let (explicit, material) = owner
                .select(&remote, account, 1, Some(prefix))
                .map_err(|_| "explicit_selection")?;
            check(explicit == binding, "explicit_selection_identity")?;
            drop(material);
            items.delete(&basic)?;
            check(
                owner
                    .candidate_choices(&remote, account)
                    .map_err(|_| "selected_delete_remaining")?
                    .len()
                    == 1,
                "selected_delete_remaining",
            )?;
            owner
                .revalidate_binding(&binding)
                .map_err(|_| "selected_delete_preserves_other")?;
            let next_secret = secret()?;
            items.update(
                &reference,
                unsafe { kSecValueData },
                CFData::from_buffer(&next_secret).into_CFType(),
            )?;
            check(
                owner.revalidate_binding(&binding).is_err(),
                "secret_only_drift",
            )?;
            let (updated, material) = owner
                .select(&remote, account, 1, None)
                .map_err(|_| "secret_rotation_selection")?;
            drop(material);
            check(
                updated.credential_item_id == binding.credential_item_id
                    && updated.credential_item_version != binding.credential_item_version,
                "secret_rotation_version",
            )?;
            for domain in [true, false] {
                let mut changed = updated.clone();
                let attribute = if domain {
                    &mut changed.security_domain
                } else {
                    &mut changed.access_group
                };
                *attribute = match attribute {
                    OptionalAttributeHashV1::Absent => OptionalAttributeHashV1::Present {
                        sha256: "0".repeat(64),
                    },
                    OptionalAttributeHashV1::Present { .. } => OptionalAttributeHashV1::Absent,
                };
                check(
                    owner.revalidate_binding(&changed).is_err(),
                    "attribute_presence_drift",
                )?;
            }
            owner
                .revalidate_binding(&updated)
                .map_err(|_| "presence_baseline_unchanged")?;
            for (field, changed, original) in [
                (
                    unsafe { kSecAttrServer },
                    CFString::new(&format!("drift-{host}")).into_CFType(),
                    CFString::new(&host).into_CFType(),
                ),
                (
                    unsafe { kSecAttrPort },
                    CFNumber::from(444_i32).into_CFType(),
                    CFNumber::from(443_i32).into_CFType(),
                ),
                (
                    unsafe { kSecAttrPath },
                    CFString::new("/owned/other").into_CFType(),
                    CFString::new("/owned/p1").into_CFType(),
                ),
                (
                    unsafe { kSecAttrAccount },
                    CFString::new("owned-other").into_CFType(),
                    CFString::new(account).into_CFType(),
                ),
                (
                    unsafe { kSecAttrProtocol },
                    value(unsafe { kSecAttrProtocolHTTP }),
                    value(unsafe { kSecAttrProtocolHTTPS }),
                ),
                (
                    unsafe { kSecAttrAuthenticationType },
                    value(unsafe { kSecAttrAuthenticationTypeHTTPBasic }),
                    value(unsafe { kSecAttrAuthenticationTypeDefault }),
                ),
                (
                    unsafe { kSecAttrSecurityDomain },
                    CFString::new("owned-nonempty").into_CFType(),
                    CFString::new("").into_CFType(),
                ),
            ] {
                let (baseline, material) = owner
                    .select(&remote, account, 1, None)
                    .map_err(|_| "attribute_baseline")?;
                drop(material);
                owner
                    .revalidate_binding(&baseline)
                    .map_err(|_| "attribute_baseline_revalidation")?;
                items.update(&reference, field, changed)?;
                check(
                    owner.revalidate_binding(&baseline).is_err(),
                    "attribute_drift",
                )?;
                items.update(&reference, field, original)?;
                let (restored, material) = owner
                    .select(&remote, account, 1, None)
                    .map_err(|_| "attribute_restore_selection")?;
                drop(material);
                let (actual_domain, actual_group) = items.actual_optional_attributes(&reference)?;
                check(
                    restored.security_domain == actual_domain
                        && restored.access_group == actual_group,
                    "attribute_restore_snapshot",
                )?;
                owner
                    .revalidate_binding(&restored)
                    .map_err(|_| "attribute_restore_revalidation")?;
            }
            // ThisDeviceOnly fixtures cannot synchronize. Do not create an iCloud
            // item merely to test a policy that permits only non-sync credentials.
            let (before_delete, material) = owner
                .select(&remote, account, 1, None)
                .map_err(|_| "sync_baseline")?;
            drop(material);
            let mut sync_binding = before_delete.clone();
            sync_binding.synchronizable = true;
            check(
                owner.revalidate_binding(&sync_binding).is_err(),
                "synchronizable_binding_policy",
            )?;
            owner
                .revalidate_binding(&before_delete)
                .map_err(|_| "synchronizable_baseline_unchanged")?;
            items.delete(&reference)?;
            check(
                owner.revalidate_binding(&before_delete).is_err(),
                "item_missing",
            )?;
            let recreated_reference = items.add(false, None)?;
            let (recreated, material) = owner
                .select(&remote, account, 2, None)
                .map_err(|_| "item_recreate_selection")?;
            drop(material);
            check(
                recreated.credential_item_id != updated.credential_item_id,
                "item_recreate_identity",
            )?;
            items.delete(&recreated_reference)?;
            let empty_domain_reference = items.add(false, Some(""))?;
            let (empty_domain_binding, material) = owner
                .select(&remote, account, 3, None)
                .map_err(|_| "empty_domain_selection")?;
            drop(material);
            let (actual_domain, actual_group) =
                items.actual_optional_attributes(&empty_domain_reference)?;
            check(
                empty_domain_binding.security_domain == actual_domain
                    && empty_domain_binding.access_group == actual_group,
                "empty_domain_snapshot_binding",
            )?;
            check(
                matches!(actual_domain, OptionalAttributeHashV1::Present { ref sha256 }
            if *sha256 == digest::digest(&digest::SHA256, b"").as_ref().iter().map(|byte| format!("{byte:02x}")).collect::<String>()),
                "empty_domain_present_hash",
            )?;
            owner
                .revalidate_binding(&empty_domain_binding)
                .map_err(|_| "empty_domain_revalidation")?;
            items.delete(&empty_domain_reference)?;
            for index in 0..65 {
                items.add(false, Some(&format!("owned-domain-{index}")))?;
            }
            check(
                owner.candidate_choices(&remote, account).is_err(),
                "candidate_limit",
            )?;
            items.cleanup()?;
            check(
                owner
                    .candidate_choices(&remote, account)
                    .map_err(|_| "cleanup_observation")?
                    .is_empty(),
                "cleanup_observation",
            )?;
            println!(
                "{}",
                serde_json::json!({"status":"passed","candidate_counts":[0,1,2],
            "explicit_selection":true,"secret_only_drift":true,"attribute_drift":true,"attribute_presence_drift":true,
            "actual_dp_attribute_snapshots":true,"empty_domain_present_hash":true,"no_domain_returned_present":no_domain_returned_present,"synchronizable_binding_policy":true,"native_synchronizable_mutation":false,"item_missing":true,"item_recreate":true,"candidate_limit":true,"owned_cleanup":true})
            );
            Ok(())
        })();
        let cleanup = items.cleanup();
        if result.is_err() || cleanup.is_err() {
            println!(
                "{}",
                serde_json::json!({
                    "status":"failed",
                    "run_failure_category":result.as_ref().err().copied(),
                    "tracked_owned_cleanup_confirmed":cleanup.is_ok(),
                    "secret_output_persisted":false
                })
            );
        }
        cleanup.map_err(|_| "owned_failure_cleanup")?;
        result
    }
}
#[cfg(target_os = "macos")]
fn main() {
    if let Err(category) = probe::run() {
        eprintln!("owned_dp_probe_failed:{category}");
        std::process::exit(1);
    }
}
