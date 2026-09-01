use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::PathBuf;

use crate::rpm::advisory::model::{Advisory, EV, LAST_UNTRACKED_PACKAGE_REMOVAL_DATE};
use edo::environment::Vfs;
use rpm::rpm_evr_compare;

use super::error::{self, Result};
use super::model::{Epoch, PackageName, PackageVersionManifest};
use snafu::{ResultExt, ensure};
use tracing::{error, info};

async fn get_rpm_metadata(spec_path: &Vfs) -> Result<PackageVersionManifest> {
    let pkg_prefix = spec_path
        .output("rpm_metadata", "rpm", &["--eval", "%{_cross_os}"])
        .await?;
    let pkg_prefix = String::from_utf8_lossy(&pkg_prefix);
    let pkg_prefix = pkg_prefix.trim();
    let rpm_metadata = spec_path
        .output(
            "rpmspec",
            "rpmspec",
            &[
                "-q",
                "--qf",
                "%{Name}|%{Epoch}|%{Version}\n",
                spec_path.as_ref(),
            ],
        )
        .await?;
    let rpm_metadata = String::from_utf8_lossy(&rpm_metadata);

    parse_rpm_metadata(pkg_prefix.to_string(), rpm_metadata.to_string())
}

fn parse_rpm_metadata(pkg_prefix: String, rpm_metadata: String) -> Result<PackageVersionManifest> {
    let mut manifest = PackageVersionManifest::new();
    for metadata in rpm_metadata.lines() {
        let parts: Vec<&str> = metadata.split('|').collect();
        ensure!(
            parts.len() == 3,
            error::RpmSpecFormatSnafu {
                spec_output: metadata
            }
        );

        let name = parts[0].strip_prefix(&pkg_prefix).unwrap_or(parts[0]);
        let epoch_str = if parts[1] == "(none)" { "0" } else { parts[1] };
        let epoch = Epoch::try_new(epoch_str).expect("epoch should be valid integer");
        let version = parts[2];

        manifest.insert(PackageName::new(name), EV::new(&epoch, version));
    }

    Ok(manifest)
}

async fn find_spec_file(package_dir: &Vfs) -> Result<Option<PathBuf>> {
    Ok(package_dir
        .find_files(".", "*.spec", false)
        .await?
        .first()
        .cloned())
}

async fn build_all_package_metadata(packages_dir: &Vfs) -> Result<PackageVersionManifest> {
    let mut manifest = PackageVersionManifest::new();

    let spec_files = packages_dir.find_files_recursive(".", "*.spec").await?;

    for entry in spec_files.iter() {
        let vfs = packages_dir.entry(entry).await;
        let pkg_manifest = get_rpm_metadata(&vfs).await?;
        manifest.merge(pkg_manifest);
    }

    Ok(manifest)
}

async fn collect_advisories(advisories_dir: &Vfs) -> Result<HashMap<PackageName, Vec<Advisory>>> {
    let mut advisories_by_product: HashMap<PackageName, Vec<Advisory>> = HashMap::new();

    for entry in advisories_dir.list(".").await? {
        for brsa in advisories_dir.list(entry).await? {
            if brsa.extension() == Some(OsStr::new("toml")) {
                let content = advisories_dir.read(&brsa).await?;
                let advisory: Advisory = toml::from_slice(&content)
                    .context(error::ParseAdvisorySnafu { path: brsa.clone() })?;
                for product in advisory.advisory_info.products.iter() {
                    advisories_by_product
                        .entry(product.package_name.clone())
                        .or_default()
                        .push(advisory.clone());
                }
            }
        }
    }

    Ok(advisories_by_product)
}

pub async fn validate_advisories(advisories_dir: &Vfs, packages_dir: &Vfs) -> Result<()> {
    // We go through all the packages in package directory and create a map with
    // package as key and Epoch:Version as value. We create a separate map from
    // the advisories with package as key and an array of advisories as value.
    let pkg_metadata = build_all_package_metadata(packages_dir).await?;
    let advisories_by_product = collect_advisories(advisories_dir).await?;

    let mut violations = Vec::new();

    // For each advisory, we check if the corresponding package exists and the
    // package is patched or if its an end of life advisory.
    for (package_name, mut advisories) in advisories_by_product {
        advisories.sort_by(|a, b| a.update_info.cmp(&b.update_info));

        if let Some(spec_ev) = pkg_metadata.get(&package_name) {
            // Package exists - check version compliance
            for advisory in advisories.iter() {
                for product in advisory.advisory_info.products.iter() {
                    if product.package_name != package_name {
                        continue;
                    }
                    let advisory_ev = EV::new(&product.patched_epoch, &product.patched_version);
                    if rpm_evr_compare(&spec_ev.to_string(), &advisory_ev.to_string()).is_lt() {
                        violations.push(format!(
                            "BRSA ID: {}\n package name: {}\n package version: {spec_ev}\n min. expected version: {advisory_ev}",
                            advisory.advisory_info.id, package_name
                        ));
                    }
                }
            }
        } else {
            // Package missing - check end-of-life
            if let Some(latest) = advisories.last() {
                let issue_date = latest.update_info.issue_date.to_string();

                // If an advisory is created after LAST_UNTRACKED_PACKAGE_REMOVAL_DATE we make sure that
                // either a package exists in the kit for that advisory or there is an end of life
                // advisory. If this test fails, it indicates that there is a typo in the package name.
                if issue_date.as_str() > LAST_UNTRACKED_PACKAGE_REMOVAL_DATE
                    && !latest.advisory_info.end_of_life
                {
                    violations.push(format!(
                        "Advisory Product '{}' has no package and latest advisory '{}' is not end-of-life",
                        package_name, latest.advisory_info.id
                    ));
                }
            }
        }
    }

    if !violations.is_empty() {
        error!(
            subsystem = "advisory",
            component = "checker",
            "Advisory violations found:\n{}",
            violations.join("\n")
        );
        return error::AdvisoryViolationsSnafu { violations }.fail();
    }
    info!(
        subsystem = "advisory",
        component = "checker",
        "All advisories validated. No errors were found"
    );
    Ok(())
}
