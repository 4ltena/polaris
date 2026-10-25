//! Bundled reviewed bytes. Callers cannot substitute project/user profiles.
use crate::{Error, Profile};

pub const MANIFEST: &[u8] = include_bytes!("../resources/nextjs/package.json");
pub const LOCK: &[u8] = include_bytes!("../resources/nextjs/package-lock.json");
pub const TEMPLATE: &[(&str, &[u8])] = &[
    ("package.json", MANIFEST),
    ("package-lock.json", LOCK),
    (
        "app/layout.js",
        include_bytes!("../resources/nextjs/layout.js"),
    ),
    ("app/page.js", include_bytes!("../resources/nextjs/page.js")),
];

pub fn nextjs() -> Result<Profile, Error> {
    let profile: Profile =
        serde_json::from_slice(include_bytes!("../resources/nextjs/profile.json"))
            .map_err(|_| Error::InvalidProfile)?;
    profile.validate()?;
    Ok(profile)
}

impl crate::Candidate {
    pub(crate) fn validate_lock(&self) -> Result<(), Error> {
        let candidate = self;
        let lock: serde_json::Value =
            serde_json::from_str(&candidate.lock).map_err(|_| Error::InvalidProfile)?;
        let manifest: serde_json::Value =
            serde_json::from_str(&candidate.manifest).map_err(|_| Error::InvalidProfile)?;
        if lock["lockfileVersion"] != 3
            || lock["packages"][""]["dependencies"] != manifest["dependencies"]
            || manifest["dependencies"]["next"] != candidate.next
        {
            return Err(Error::InvalidProfile);
        }
        let packages = lock["packages"].as_object().ok_or(Error::InvalidProfile)?;
        // npm ci validates optional platform records even when they cannot be
        // installed here. Retain their metadata without acquiring their bodies.
        let supported = packages
            .values()
            .filter(|record| {
                !(record["optional"] == true
                    && (crate::definition::excluded_platform(&record["os"], "darwin")
                        || crate::definition::excluded_platform(&record["cpu"], "arm64")))
            })
            .count();
        if supported != candidate.packages.len() + 1 {
            return Err(Error::InvalidProfile);
        }
        for artifact in &candidate.packages {
            let record = &lock["packages"][format!("node_modules/{}", artifact.name)];
            if record["version"] != artifact.version
                || record["resolved"] != artifact.url
                || !record["integrity"]
                    .as_str()
                    .is_some_and(|value| value.starts_with("sha512-"))
            {
                return Err(Error::InvalidProfile);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_profile_and_lock_agree() {
        let profile = nextjs().expect("reviewed profile");
        assert_eq!(profile.candidate("16.4.3").unwrap().next, "16.4.3");
        assert!(profile.candidate("16.4.2").is_err());
        assert!(profile.candidate("^15").is_err());
        assert!(profile.candidate("^16").is_ok());
    }

    #[test]
    fn fallback_is_compatibility_only_and_preserves_explicit_versions() {
        let mut profile = nextjs().unwrap();
        let mut alternative = profile.candidates[0].clone();
        alternative.node.version = "22.23.4".into();
        alternative.node.url = "https://nodejs.org/dist/fixed-alternative.tar.gz".into();
        profile.candidates.push(alternative);
        assert_eq!(
            profile.fallback(0, crate::Failure::RuntimeIncompatible, false, "16.4.3"),
            Ok(Some(1))
        );
        for failure in [
            crate::Failure::Transport,
            crate::Failure::Authentication,
            crate::Failure::Permission,
            crate::Failure::Disk,
            crate::Failure::Digest,
            crate::Failure::Build,
            crate::Failure::Cancelled,
        ] {
            assert_eq!(profile.fallback(0, failure, true, "*"), Ok(None));
        }
        assert_eq!(
            profile.fallback(0, crate::Failure::PrimaryIncompatible, false, "*"),
            Ok(None)
        );
        assert_eq!(
            profile.fallback(0, crate::Failure::PrimaryIncompatible, true, "16.4.3"),
            Ok(None)
        );
    }

    #[test]
    fn execution_recipe_changes_invalidate_the_profile() {
        let mut profile = nextjs().unwrap();
        profile.candidates[0]
            .execution
            .install_args
            .retain(|arg| arg != "--offline");
        assert_eq!(profile.validate(), Err(Error::InvalidProfile));
    }
}
