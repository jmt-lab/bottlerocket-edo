//! Variant-metadata RPM spec and rpmbuild argument generation.
//!
//! Consumes the bundled `metadata.spec` (see [`crate::METADATA_SPEC`])
//! along with the resolved [`ImageFeatures`] to render the `--with` /
//! `--without` argument list rpmbuild expects.

use crate::model::{ImageFeature, ImageFeatures};

/// Convert an [`ImageFeatures`] snapshot into the ordered
/// `--with FLAG` / `--without FLAG` argument list for `rpmbuild`. Every
/// feature listed in [`RPMBUILD_TOGGLES`] appears exactly once.
///
/// Matches the shell's convention: features named in `metadata.spec`
/// under `%if %{with feature_name}` translate to `--with feature_name`
/// / `--without feature_name`.
pub fn render_metadata_rpmbuild_args(features: &ImageFeatures) -> Vec<String> {
    let mut out = Vec::with_capacity(RPMBUILD_TOGGLES.len() * 2);
    for (flag, on) in metadata_toggles(features) {
        out.push(if on {
            "--with".into()
        } else {
            "--without".into()
        });
        out.push(flag.into());
    }
    out
}

/// Features that appear as `%if %{with ...}` toggles in `metadata.spec`.
///
/// The `rpmbuild_name` column is the exact identifier the spec expects;
/// the accessor produces the value from an [`ImageFeatures`] snapshot.
pub const RPMBUILD_TOGGLES: &[(ImageFeature, &str)] = &[
    (ImageFeature::InPlaceUpdates, "in_place_updates"),
    (ImageFeature::HostContainers, "host_containers"),
    (ImageFeature::UefiSecureBoot, "uefi_secure_boot"),
    (ImageFeature::XfsDataPartition, "xfs_data_partition"),
    (ImageFeature::ErofsRootPartition, "erofs_root_partition"),
    (ImageFeature::Fips, "fips"),
    (
        ImageFeature::ExternalKmodDevelopment,
        "external_kmod_development",
    ),
    (ImageFeature::EncryptedStorage, "encrypted_storage"),
    (ImageFeature::FirstPartyStack, "first_party_stack"),
];

fn metadata_toggles(features: &ImageFeatures) -> Vec<(&'static str, bool)> {
    RPMBUILD_TOGGLES
        .iter()
        .map(|(feature, name)| (*name, is_on(features, *feature)))
        .collect()
}

fn is_on(features: &ImageFeatures, feature: ImageFeature) -> bool {
    match feature {
        ImageFeature::GrubSetPrivateVar => features.grub_set_private_var,
        ImageFeature::SystemdNetworkd => features.systemd_networkd,
        ImageFeature::XfsDataPartition => features.xfs_data_partition,
        ImageFeature::ErofsRootPartition => features.erofs_root_partition,
        ImageFeature::UefiSecureBoot => features.uefi_secure_boot,
        ImageFeature::Fips => features.fips,
        ImageFeature::InPlaceUpdates => features.in_place_updates,
        ImageFeature::HostContainers => features.host_containers,
        ImageFeature::ExternalKmodDevelopment => features.external_kmod_development,
        ImageFeature::EncryptedStorage => features.encrypted_storage,
        ImageFeature::FirstPartyStack => features.first_party_stack,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_off_yields_all_without() {
        let f = ImageFeatures::default();
        let args = render_metadata_rpmbuild_args(&f);
        assert_eq!(args.len(), RPMBUILD_TOGGLES.len() * 2);
        for pair in args.chunks(2) {
            assert_eq!(pair[0], "--without");
        }
    }

    #[test]
    fn selected_features_produce_with() {
        let f = ImageFeatures {
            uefi_secure_boot: true,
            first_party_stack: true,
            in_place_updates: true,
            ..ImageFeatures::default()
        };
        let args = render_metadata_rpmbuild_args(&f);
        // Format is (--with|--without) NAME pairs; scan for our flags.
        let mut with = Vec::new();
        let mut without = Vec::new();
        for pair in args.chunks(2) {
            if pair[0] == "--with" {
                with.push(pair[1].clone());
            } else {
                without.push(pair[1].clone());
            }
        }
        assert!(with.contains(&"uefi_secure_boot".to_string()));
        assert!(with.contains(&"first_party_stack".to_string()));
        assert!(with.contains(&"in_place_updates".to_string()));
        assert!(without.contains(&"fips".to_string()));
        assert!(without.contains(&"encrypted_storage".to_string()));
    }

    #[test]
    fn embedded_metadata_spec_is_non_empty() {
        assert!(crate::METADATA_SPEC.contains("%{_cross_os}metadata"));
    }
}
