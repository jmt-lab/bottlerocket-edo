use std::collections::HashMap;
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use async_trait::async_trait;
use blake3::Hasher;
use edo::{
    context::{Addr, Context, FromNode, Handle, Log, Node},
    environment::{Environment, Vfs},
    non_configurable,
    source::Source,
    storage::{Artifact, Compression, Config, Id, MediaType},
    transform::{TransformError, TransformImpl, TransformResult, TransformStatus},
};
use edo_core::transform::parse_sources_with_name;
use indexmap::IndexMap;
use nutype::nutype;
use ocilot::models::Platform;
use regex::Regex;
use rpm::rpm_evr_compare;
use serde::Deserialize;
use snafu::{OptionExt, ResultExt};
use tokio::io::{AsyncReadExt, BufReader};
use tokio_tar::Archive;
use tracing::trace;

const LAST_UNTRACKED_PACKAGE_REMOVAL_DATE: &str = "2025-07-26";
const DEFAULT_EPOCH: &str = "0";

const PREFIX_OUT_PATH: &str = "advisory_prefix.txt";
const MANIFEST_OUT_PATH: &str = "advisory_manifest.txt";

static CVE_ID_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^CVE-[0-9]{4}-[0-9]{4,19}$"#).expect("Failed to compile CVE_ID_REGEX")
});

static GHSA_ID_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^GHSA(-[23456789cfghjmpqrvwx]{4}){3}$").expect("Failed to compile GHSA_ID_REGEX")
});

fn is_integer(s: &str) -> bool {
    s.parse::<i64>().is_ok()
}

#[nutype(
    validate(predicate = is_integer),
    default = DEFAULT_EPOCH,
    derive(Debug, Clone, Deserialize, PartialEq, Display, AsRef, Default)
)]
pub struct Epoch(String);

#[derive(Debug, Clone)]
pub struct EV {
    pub epoch: Epoch,
    pub version: String,
}

impl EV {
    pub fn new(epoch: &Epoch, version: impl Into<String>) -> Self {
        Self {
            epoch: epoch.clone(),
            version: version.into(),
        }
    }
}

impl Display for EV {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.epoch, self.version)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct PackageName(String);

impl Display for PackageName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl PackageName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
}

#[derive(Debug, Default)]
pub struct PackageVersionManifest {
    packages: HashMap<PackageName, EV>,
}

impl PackageVersionManifest {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: PackageName, evr: EV) {
        self.packages.insert(name, evr);
    }

    pub fn get(&self, name: &PackageName) -> Option<&EV> {
        self.packages.get(name)
    }

    pub fn merge(&mut self, other: PackageVersionManifest) {
        self.packages.extend(other.packages);
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Advisory {
    #[serde(rename = "advisory")]
    pub advisory_info: AdvisoryInfo,
    #[serde(rename = "updateinfo")]
    pub update_info: UpdateInfo,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub struct AdvisoryInfo {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub end_of_life: bool,
    pub cve: Option<CveId>,
    pub ghsa: Option<GhsaId>,
    pub severity: Severity,
    pub description: String,
    pub products: Vec<Product>,
}

#[nutype(
    validate(regex = CVE_ID_REGEX),
    derive(Debug, Clone, Deserialize, PartialEq)
)]
pub struct CveId(String);

#[nutype(
    validate(regex = GHSA_ID_REGEX),
    derive(Debug, Clone, Deserialize, PartialEq)
)]
pub struct GhsaId(String);

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct UpdateInfo {
    pub issue_date: toml::value::Datetime,
    pub version: String,
}

impl Ord for UpdateInfo {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.issue_date
            .to_string()
            .cmp(&other.issue_date.to_string())
    }
}

impl PartialOrd for UpdateInfo {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)]
pub enum Severity {
    Low,
    #[serde(alias = "medium")]
    Moderate,
    High,
    Critical,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Product {
    pub package_name: PackageName,
    pub patched_version: String,
    #[serde(default = "default_epoch")]
    pub patched_epoch: Epoch,
}

fn default_epoch() -> Epoch {
    Epoch::default()
}

pub struct AdvisoryCheckTransform {
    pub addr: Addr,
    pub sdk: Addr,
    pub arch: Option<String>,
    pub advisories: IndexMap<String, Source>,
    pub packages: IndexMap<String, Source>,
}

#[async_trait]
impl FromNode for AdvisoryCheckTransform {
    type Error = error::Error;

    async fn from_node(addr: &Addr, node: &Node, ctx: &Context) -> Result<Self, error::Error> {
        node.validate_keys(&["sdk"])?;
        let sdk = Addr::parse(&node.get("sdk").and_then(|x| x.as_string()).context(
            error::FieldSnafu {
                field: "sdk",
                type_: "string",
            },
        )?)?;
        let field_error = |field: &str, type_: &str| error::Error::Field {
            field: field.to_string(),
            type_: type_.to_string(),
        };
        let advisories =
            parse_sources_with_name("advisories", addr, node, ctx, field_error).await?;
        let packages = parse_sources_with_name("packages", addr, node, ctx, field_error).await?;
        Ok(Self {
            addr: addr.clone(),
            sdk,
            arch: ctx
                .args()
                .get("arch")
                .cloned()
                .or(node.get("arch").and_then(|x| x.as_string())),
            advisories,
            packages,
        })
    }
}

non_configurable!(AdvisoryCheckTransform, error::Error);

#[async_trait]
impl TransformImpl for AdvisoryCheckTransform {
    async fn environment(&self) -> TransformResult<Addr> {
        Ok(self.sdk.clone())
    }

    async fn get_unique_id(&self, _ctx: &Handle) -> TransformResult<Id> {
        let mut hash = Hasher::new();
        for source in self.advisories.values() {
            hash.update(source.get_unique_id().await?.digest().as_bytes());
        }
        for source in self.packages.values() {
            hash.update(source.get_unique_id().await?.digest().as_bytes());
        }
        let digest = hash.finalize().to_hex().to_lowercase();
        let id = Id::builder()
            .name(self.addr.to_id())
            .digest(digest)
            .maybe_arch(self.arch.clone())
            .build();
        trace!(component = "transform", type = "advisory-check", "id is calculated to be {id}");
        Ok(id)
    }

    async fn depends(&self) -> TransformResult<Vec<Addr>> {
        Ok(Vec::new())
    }

    async fn prepare(&self, log: &Log, ctx: &Handle) -> TransformResult<()> {
        for source in self.advisories.values() {
            source.cache(log, ctx.storage()).await?;
        }
        for source in self.packages.values() {
            source.cache(log, ctx.storage()).await?;
        }
        Ok(())
    }

    async fn stage(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
        let advisories_root = Path::new("advisories");
        let packages_root = Path::new("packages");
        env.create_dir(advisories_root).await?;
        env.create_dir(packages_root).await?;

        for (addr, source) in self.advisories.iter() {
            trace!(component = "transform", type = "advisory-check", "staging advisory source {addr}");
            source
                .stage(log, ctx.storage(), env, advisories_root)
                .await?;
        }
        for (addr, source) in self.packages.iter() {
            trace!(component = "transform", type = "advisory-check", "staging package source {addr}");
            source
                .stage(log, ctx.storage(), env, packages_root)
                .await?;
        }
        Ok(())
    }

    async fn transform(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus {
        match async move {
            let id = self.get_unique_id(ctx).await?;
            let vfs = Vfs::new(&id, env, log).await?;

            // Step 1: emit %{_cross_os} prefix to a known file inside the env.
            vfs.command(
                "rpm-prefix",
                "sh",
                &[
                    "-c",
                    &format!("'rpm --eval %{{_cross_os}} > {PREFIX_OUT_PATH}'"),
                ],
            )
            .await?;

            // Step 2: collect rpmspec metadata for every spec under packages/
            // into a single file. Use sh -c so we can pipe through xargs and
            // redirect; quote the qf format so shell does not split on `|`.
            vfs.command(
                "rpmspec-collect",
                "sh",
                &[
                    "-c",
                    &format!(
                        "'find packages -name *.spec -print0 | xargs -0 -r rpmspec -q --qf \"%{{Name}}|%{{Epoch}}|%{{Version}}\\n\" > {MANIFEST_OUT_PATH}'"
                    ),
                ],
            )
            .await?;

            // Step 3: pull the prefix and manifest files out of the env.
            let prefix = read_env_file(ctx, env, Path::new(PREFIX_OUT_PATH)).await?;
            let manifest_text = read_env_file(ctx, env, Path::new(MANIFEST_OUT_PATH)).await?;

            // Step 4: parse rpm metadata and load advisories.
            let manifest = parse_rpm_metadata(prefix.trim().to_string(), manifest_text)?;
            let advisories_by_product = read_advisories(ctx, env).await?;

            // Step 5: pure validation logic.
            let violations = check(&manifest, advisories_by_product);
            if !violations.is_empty() {
                return Err(error::Error::Violations { violations });
            }

            // Marker artifact with no layers.
            let artifact = Artifact::builder()
                .config(Config::builder().id(id.clone()).build())
                .media_type(MediaType::Manifest)
                .build();
            ctx.storage().safe_save(&artifact).await?;
            Ok::<Artifact, error::Error>(artifact)
        }
        .await
        {
            Ok(artifact) => TransformStatus::Success(artifact),
            Err(e) => TransformStatus::Failed(Some(log.path()), e.into()),
        }
    }

    fn can_shell(&self) -> bool {
        true
    }

    fn shell(&self, env: &Environment) -> TransformResult<()> {
        env.shell(Path::new("."))?;
        Ok(())
    }
}

fn parse_rpm_metadata(
    pkg_prefix: String,
    rpm_metadata: String,
) -> Result<PackageVersionManifest, error::Error> {
    let mut manifest = PackageVersionManifest::new();
    for metadata in rpm_metadata.lines() {
        let parts: Vec<&str> = metadata.split('|').collect();
        snafu::ensure!(
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

fn check(
    pkg_metadata: &PackageVersionManifest,
    advisories_by_product: HashMap<PackageName, Vec<Advisory>>,
) -> Vec<String> {
    let mut violations = Vec::new();
    for (package_name, mut advisories) in advisories_by_product {
        advisories.sort_by(|a, b| a.update_info.cmp(&b.update_info));

        if let Some(spec_ev) = pkg_metadata.get(&package_name) {
            for advisory in &advisories {
                for product in &advisory.advisory_info.products {
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
        } else if let Some(latest) = advisories.last() {
            let issue_date = latest.update_info.issue_date.to_string();
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
    violations
}

async fn read_env_file(
    ctx: &Handle,
    env: &Environment,
    path: &Path,
) -> Result<String, error::Error> {
    // env.read tars the requested path into a writer-backed layer; we then
    // read that layer back from storage and untar to extract the single file.
    let writer = ctx.storage().safe_start_layer().await?;
    env.read(path, writer.clone()).await?;
    let layer = ctx
        .storage()
        .safe_finish_layer(&MediaType::Tar(Compression::None), None, &writer)
        .await?;
    let reader = ctx.storage().safe_read(&layer).await?;
    let mut archive = Archive::new(BufReader::new(reader));
    let mut entries = archive.entries().context(error::ReadEnvSnafu {
        path: path.to_path_buf(),
    })?;
    use tokio_stream::StreamExt;
    while let Some(entry) = entries.next().await {
        let mut entry = entry.context(error::ReadEnvSnafu {
            path: path.to_path_buf(),
        })?;
        let mut buf = String::new();
        entry
            .read_to_string(&mut buf)
            .await
            .context(error::ReadEnvSnafu {
                path: path.to_path_buf(),
            })?;
        return Ok(buf);
    }
    Ok(String::new())
}

async fn read_advisories(
    ctx: &Handle,
    env: &Environment,
) -> Result<HashMap<PackageName, Vec<Advisory>>, error::Error> {
    let writer = ctx.storage().safe_start_layer().await?;
    env.read(Path::new("advisories"), writer.clone()).await?;
    let layer = ctx
        .storage()
        .safe_finish_layer(&MediaType::Tar(Compression::None), None, &writer)
        .await?;
    let reader = ctx.storage().safe_read(&layer).await?;
    let mut archive = Archive::new(BufReader::new(reader));
    let mut entries = archive.entries().context(error::ReadEnvSnafu {
        path: PathBuf::from("advisories"),
    })?;

    use tokio_stream::StreamExt;
    let mut advisories_by_product: HashMap<PackageName, Vec<Advisory>> = HashMap::new();
    while let Some(entry) = entries.next().await {
        let mut entry = entry.context(error::ReadEnvSnafu {
            path: PathBuf::from("advisories"),
        })?;
        let entry_path = entry
            .path()
            .context(error::ReadEnvSnafu {
                path: PathBuf::from("advisories"),
            })?
            .into_owned();
        if entry_path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        let mut buf = String::new();
        entry
            .read_to_string(&mut buf)
            .await
            .context(error::ReadEnvSnafu {
                path: entry_path.clone(),
            })?;
        let advisory: Advisory =
            toml::from_str(&buf).context(error::ParseAdvisorySnafu { path: entry_path })?;
        for product in &advisory.advisory_info.products {
            advisories_by_product
                .entry(product.package_name.clone())
                .or_default()
                .push(advisory.clone());
        }
    }
    Ok(advisories_by_product)
}

pub mod error {
    use edo::{
        context::ContextError, source::SourceError, storage::StorageError,
        transform::TransformError,
    };
    use snafu::Snafu;
    use std::path::PathBuf;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(crate)))]
    pub enum Error {
        #[snafu(transparent)]
        Context { source: ContextError },
        #[snafu(transparent)]
        Storage { source: StorageError },
        #[snafu(transparent)]
        Source { source: SourceError },
        #[snafu(transparent)]
        Environment {
            source: edo::environment::EnvironmentError,
        },
        #[snafu(display(
            "advisory-check transform expects a field '{field}' with type '{type_}'"
        ))]
        Field { field: String, type_: String },
        #[snafu(display(
            "Incorrect output format for rpmspec command. Expected name|epoch|version. Got '{spec_output}'"
        ))]
        RpmSpecFormat { spec_output: String },
        #[snafu(display("Failed to parse advisory at '{}'", path.display()))]
        ParseAdvisory {
            path: PathBuf,
            source: toml::de::Error,
        },
        #[snafu(display("Failed to read '{}' from environment", path.display()))]
        ReadEnv {
            path: PathBuf,
            source: std::io::Error,
        },
        #[snafu(display("{} advisory violations found:\n{}", violations.len(), violations.join("\n")))]
        Violations { violations: Vec<String> },
    }

    impl From<Error> for TransformError {
        fn from(value: Error) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }

    impl From<Error> for ContextError {
        fn from(value: Error) -> Self {
            Self::Component {
                source: Box::new(value),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn make_advisory_toml(id_field: &str, id_value: &str) -> String {
        format!(
            r#"
[advisory]
id = "TEST-001"
title = "Test Advisory"
severity = "high"
description = "Test description"
{id_field} = "{id_value}"
[[advisory.products]]
package-name = "test"
patched-version = "1.0"

[updateinfo]
issue-date = 2024-01-01
version = "1"
"#
        )
    }

    #[test_case("CVE-2024-1234"; "basic 4 digit")]
    #[test_case("CVE-2024-12345"; "5 digit")]
    #[test_case("CVE-2025-472688"; "6 digit")]
    #[test_case("CVE-1999-0001"; "old year")]
    fn valid_cve(cve: &str) {
        let toml = make_advisory_toml("cve", cve);
        assert!(
            toml::from_str::<Advisory>(&toml).is_ok(),
            "Failed to parse valid CVE: {cve}"
        );
    }

    #[test_case("CVE-2024-001"; "too few digits")]
    #[test_case("CVE-2024-INVALID"; "non numeric")]
    #[test_case("CVE-24-1234"; "year too short")]
    #[test_case("CVE-2024-0"; "single digit")]
    #[test_case("CVE\u{2013}2024\u{2013}1234"; "en dash")]
    #[test_case("CVE\u{2014}2024\u{2014}1234"; "em dash")]
    #[test_case("GHSA-xxxx-xxxx-xxxx"; "wrong format")]
    fn invalid_cve(cve: &str) {
        let toml = make_advisory_toml("cve", cve);
        assert!(
            toml::from_str::<Advisory>(&toml).is_err(),
            "Should reject invalid CVE: {cve}"
        );
    }

    #[test_case("GHSA-23fg-6c23-wxrv"; "standard")]
    #[test_case("GHSA-2222-3333-4444"; "all numeric")]
    #[test_case("GHSA-cfgh-jmpq-rvwx"; "all alpha")]
    fn valid_ghsa(ghsa: &str) {
        let toml = make_advisory_toml("ghsa", ghsa);
        assert!(
            toml::from_str::<Advisory>(&toml).is_ok(),
            "Failed to parse valid GHSA: {ghsa}"
        );
    }

    #[test_case("GHSA-123-456-789"; "too short")]
    #[test_case("GHSA-12345-67890-12345"; "too long")]
    #[test_case("CVE-2024-1234"; "wrong format")]
    fn invalid_ghsa(ghsa: &str) {
        let toml = make_advisory_toml("ghsa", ghsa);
        assert!(
            toml::from_str::<Advisory>(&toml).is_err(),
            "Should reject invalid GHSA: {ghsa}"
        );
    }

    #[test]
    fn complete_advisory() {
        let toml = make_advisory_toml("cve", "CVE-2025-12345");
        let advisory: Advisory = toml::from_str(&toml).expect("Failed to parse complete advisory");
        assert_eq!(advisory.advisory_info.id, "TEST-001");
        assert_eq!(
            advisory.advisory_info.cve,
            CveId::try_new("CVE-2025-12345").ok()
        );
        assert_eq!(advisory.advisory_info.products.len(), 1);
        assert_eq!(
            advisory.advisory_info.products[0].package_name,
            PackageName::new("test")
        );
        assert_eq!(
            advisory.advisory_info.products[0].patched_epoch,
            Epoch::try_new("0").unwrap()
        );
    }

    #[test_case("testpkg|0|1.0.0\n"; "Epoch present")]
    #[test_case("testpkg|(none)|1.0.0\n"; "Epoch not present")]
    #[test_case("testpkg|(none)|1.0.0\ntestpkg-bin|(none)|1.0.0\ntestpkg-lib|(none)|1.0.0\n"; "Multiple sub-packages")]
    fn parse_rpm_metadata_basic(rpm_metadata: &str) {
        let output = rpm_metadata.to_string();
        let result = parse_rpm_metadata("".to_string(), output).unwrap();
        let evr = result.get(&PackageName::new("testpkg")).unwrap();
        assert_eq!(evr.to_string(), "0:1.0.0");
    }

    #[test_case("testpkg\n"; "invalid output 1")]
    #[test_case("testpkg|(none)\n"; "invalid output 2")]
    fn parse_invalid_rpm_metadata(rpm_metadata: &str) {
        let result = parse_rpm_metadata("".to_string(), rpm_metadata.to_string());
        assert!(result.is_err());
    }

    #[test]
    fn parse_rpm_metadata_with_prefix() {
        let output = "bottlerocket-testpkg|1|3.0.0\n".to_string();
        let result = parse_rpm_metadata("bottlerocket-".to_string(), output).unwrap();
        let evr = result.get(&PackageName::new("testpkg")).unwrap();
        assert_eq!(evr.to_string(), "1:3.0.0");
    }

    #[test]
    fn parse_rpm_metadata_multiple_packages() {
        let output = "pkg1|0|1.0\npkg2|1|2.0\n".to_string();
        let result = parse_rpm_metadata("".to_string(), output).unwrap();
        assert_eq!(
            result.get(&PackageName::new("pkg1")).unwrap().to_string(),
            "0:1.0"
        );
        assert_eq!(
            result.get(&PackageName::new("pkg2")).unwrap().to_string(),
            "1:2.0"
        );
    }

    fn advisory_for(
        id: &str,
        package: &str,
        patched_version: &str,
        patched_epoch: &str,
        issue_date: &str,
        end_of_life: bool,
    ) -> Advisory {
        let toml_str = format!(
            r#"
[advisory]
id = "{id}"
title = "t"
severity = "high"
description = "d"
end-of-life = {end_of_life}
[[advisory.products]]
package-name = "{package}"
patched-version = "{patched_version}"
patched-epoch = "{patched_epoch}"

[updateinfo]
issue-date = {issue_date}
version = "1"
"#
        );
        toml::from_str(&toml_str).unwrap()
    }

    #[test]
    fn check_no_violations_when_patched() {
        let mut manifest = PackageVersionManifest::new();
        manifest.insert(
            PackageName::new("foo"),
            EV::new(&Epoch::try_new("0").unwrap(), "2.0.0"),
        );
        let mut advisories = HashMap::new();
        advisories.insert(
            PackageName::new("foo"),
            vec![advisory_for(
                "BRSA-1",
                "foo",
                "1.0.0",
                "0",
                "2025-08-01",
                false,
            )],
        );
        assert!(check(&manifest, advisories).is_empty());
    }

    #[test]
    fn check_reports_outdated_package() {
        let mut manifest = PackageVersionManifest::new();
        manifest.insert(
            PackageName::new("foo"),
            EV::new(&Epoch::try_new("0").unwrap(), "1.0.0"),
        );
        let mut advisories = HashMap::new();
        advisories.insert(
            PackageName::new("foo"),
            vec![advisory_for(
                "BRSA-2",
                "foo",
                "2.0.0",
                "0",
                "2025-08-01",
                false,
            )],
        );
        let violations = check(&manifest, advisories);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("BRSA-2"));
    }

    #[test]
    fn check_missing_package_post_cutoff_not_eol_is_violation() {
        let manifest = PackageVersionManifest::new();
        let mut advisories = HashMap::new();
        advisories.insert(
            PackageName::new("ghost"),
            vec![advisory_for(
                "BRSA-3",
                "ghost",
                "1.0.0",
                "0",
                "2025-08-01",
                false,
            )],
        );
        let violations = check(&manifest, advisories);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("ghost"));
    }

    #[test]
    fn check_missing_package_eol_is_ok() {
        let manifest = PackageVersionManifest::new();
        let mut advisories = HashMap::new();
        advisories.insert(
            PackageName::new("ghost"),
            vec![advisory_for(
                "BRSA-4",
                "ghost",
                "1.0.0",
                "0",
                "2025-08-01",
                true,
            )],
        );
        assert!(check(&manifest, advisories).is_empty());
    }

    #[test]
    fn check_missing_package_pre_cutoff_is_ok() {
        let manifest = PackageVersionManifest::new();
        let mut advisories = HashMap::new();
        advisories.insert(
            PackageName::new("ghost"),
            vec![advisory_for(
                "BRSA-5",
                "ghost",
                "1.0.0",
                "0",
                "2025-01-01",
                false,
            )],
        );
        assert!(check(&manifest, advisories).is_empty());
    }
}
