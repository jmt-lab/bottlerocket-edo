//! `kit-image` vendor: exposes a Bottlerocket OCI registry as an edo
//! [`Vendor`](edo::source::Vendor). Mirrors edo-core's `ImageVendor` for
//! tag/version discovery and merkle-digest resolution, but with two
//! kit-specific behaviours layered on top of the underlying registry:
//!
//! * [`VendorImpl::get_options`] filters candidate tags to those whose
//!   image config carries the `dev.bottlerocket.kit.v3` metadata label.
//!   This prevents non-kit images (e.g. the SDK) from being surfaced as
//!   kit candidates when the same registry hosts both kit and non-kit
//!   artifacts, which would otherwise cause resolvo to see the same
//!   version numbers under both the kit and SDK vendors and pick a
//!   nonsense assignment.
//! * [`VendorImpl::get_dependencies`] decodes the same label and
//!   surfaces the kit's transitive SDK + kit requirements to resolvo.

use async_trait::async_trait;
use edo::context::{Addr, Context, Element, FromElement};
use edo::source::{SourceResult, VendorImpl};
use futures::stream::{self, StreamExt};
use ocilot::index::Index;
use ocilot::models::{ErrorCode, ErrorResponse};
use ocilot::registry::Registry;
use ocilot::repository::Repository;
use ocilot::uri::{Reference, RegistryUri, Uri};
use semver::{Version, VersionReq};
use serde_json::json;
use sha2::{Digest, Sha256};
use snafu::{ResultExt, ensure};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::time::Duration;

/// Maximum concurrent kit-metadata probes against a single registry.
///
/// Sized to stay well under ECR Public's per-source request budget; a
/// higher value trades discovery latency for a much higher chance of
/// rate-limit-driven false negatives.
const MAX_CONCURRENT_METADATA_PROBES: usize = 4;

/// Number of times a rate-limited registry probe is retried before we
/// give up and treat the tag as "not a kit image".
const RATE_LIMIT_RETRIES: usize = 4;

/// Initial retry delay after a rate-limit response; doubled after each
/// subsequent failure (capped at [`RATE_LIMIT_MAX_DELAY`]).
const RATE_LIMIT_INITIAL_DELAY: Duration = Duration::from_millis(250);

/// Upper bound on the exponential backoff delay so a very heavy repo
/// can't stretch discovery past a reasonable ceiling.
const RATE_LIMIT_MAX_DELAY: Duration = Duration::from_secs(4);

use super::kit_image::{KIT_METADATA_LABEL, KitMetadata};

#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct KitImageVendorOptions {
    /// Base registry URI (e.g. `public.ecr.aws/bottlerocket`).
    uri: String,
    /// Optional friendly name for diagnostics; unused today but reserved
    /// so `[vendor.<name>]` can carry the twoliter vendor label into
    /// diagnostics without a downstream break.
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
}

/// A vendor that serves Bottlerocket kits from an OCI registry.
pub struct KitImageVendor {
    registry: Registry,
}

// `Registry` from `ocilot` is `!Send`/`!Sync` conservatively but its
// interior state is only touched behind async locks; matches the manual
// impls edo-core uses for `ImageVendor`.
unsafe impl Send for KitImageVendor {}
unsafe impl Sync for KitImageVendor {}

#[async_trait]
impl FromElement for KitImageVendor {
    type Error = error::Error;

    async fn new(element: &Element, _ctx: &Context) -> Result<Self, error::Error> {
        let options: KitImageVendorOptions = element.get()?;
        let registry_uri = RegistryUri::from_str(&options.uri).context(error::OciSnafu)?;
        Ok(Self {
            registry: Registry::new(&registry_uri)
                .await
                .context(error::OciSnafu)?,
        })
    }
}

#[async_trait]
impl VendorImpl for KitImageVendor {
    async fn get_options(&self, name: &str) -> SourceResult<HashSet<Version>> {
        // 1. Enumerate all semver-shaped tags in the repo, tolerating an
        //    optional `v` prefix (Bottlerocket kits are published as
        //    `vX.Y.Z`, but semver-parseable bare tags are accepted too).
        let repo = Repository::new(&self.registry, name);
        let mut candidates: HashSet<Version> = HashSet::new();
        for tag in repo.tags().await.context(error::OciSnafu)? {
            let stag = tag.strip_prefix('v').unwrap_or(tag.as_str());
            if let Ok(version) = Version::parse(stag) {
                candidates.insert(version);
            }
        }

        // 2. Filter to versions whose image config carries the kit
        //    metadata label. Non-kit images (e.g. the SDK, which shares
        //    the same registry in the common Bottlerocket setup) parse
        //    as valid semver tags but must NOT be reported as kit
        //    candidates: otherwise resolvo would see the same versions
        //    under both the `kit-image` and `image` vendors and could
        //    pick an SDK version as a kit resolution.
        //
        //    Fanned out with [`MAX_CONCURRENT_METADATA_PROBES`] in-flight
        //    at a time to keep discovery latency close to the single-image
        //    case without tripping the registry's rate limiter (ECR Public
        //    returns `TOOMANYREQUESTS` under an unbounded fanout, which
        //    silently dropped tags and made resolution non-deterministic).
        //    Per-tag failures are logged inside `has_kit_metadata` and
        //    drop the tag rather than aborting the whole discovery.
        let versions: HashSet<Version> = stream::iter(candidates)
            .map(|version| async move {
                let has_label = self.has_kit_metadata(name, &version).await;
                (version, has_label)
            })
            .buffer_unordered(MAX_CONCURRENT_METADATA_PROBES)
            .filter_map(|(version, has_label)| async move {
                if has_label { Some(version) } else { None }
            })
            .collect()
            .await;
        Ok(versions)
    }

    async fn resolve(&self, name: &str, version: &Version) -> SourceResult<Element> {
        let (uri, index) = self.fetch_index(name, version).await?;
        let mut hasher = Sha256::new();
        for manifest in index.manifests().iter() {
            hasher.update(manifest.digest().as_bytes());
        }
        let digest = base16::encode_lower(hasher.finalize().as_slice());
        Ok(Element::builder()
            .addr(Addr::parse(name)?)
            .kind("kit-image")
            .config([
                ("uri".to_string(), json!(uri.to_string())),
                ("ref".to_string(), json!(digest)),
            ])
            .build())
    }

    async fn get_dependencies(
        &self,
        name: &str,
        version: &Version,
    ) -> SourceResult<Option<HashMap<String, VersionReq>>> {
        // Fetch the manifest list and the first image's config blob.
        // Any missing / malformed label is treated as "no known deps"
        // (Ok(None)) to match edo-core's behaviour for non-edo artifacts,
        // but is emitted as a warning so misconfigured images are visible.
        let (uri, index) = match self.fetch_index(name, version).await {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!(
                    subsystem = "vendor",
                    component = "kit-image",
                    %name,
                    %version,
                    error = %err,
                    "failed to fetch manifest index; treating as no deps",
                );
                return Ok(None);
            }
        };
        let image = match index
            .fetch_image(&uri, None)
            .await
            .context(error::OciSnafu)?
        {
            Some(image) => image,
            None => {
                tracing::warn!(
                    subsystem = "vendor",
                    component = "kit-image",
                    %name,
                    %version,
                    "kit image has no platform-selectable manifest",
                );
                return Ok(None);
            }
        };
        let config = image.fetch_config(&uri).await.context(error::OciSnafu)?;
        let label = match config.config.labels.get(KIT_METADATA_LABEL) {
            Some(label) => label,
            None => {
                tracing::warn!(
                    subsystem = "vendor",
                    component = "kit-image",
                    %name,
                    %version,
                    label = KIT_METADATA_LABEL,
                    "kit image is missing metadata label; treating as no deps",
                );
                return Ok(None);
            }
        };
        let metadata = match KitMetadata::from_label(label) {
            Ok(m) => m,
            Err(err) => {
                tracing::warn!(
                    subsystem = "vendor",
                    component = "kit-image",
                    %name,
                    %version,
                    error = %err,
                    "kit metadata label failed to decode; treating as no deps",
                );
                return Ok(None);
            }
        };

        // Every referenced kit + the SDK is pinned to its exact version.
        let mut deps: HashMap<String, VersionReq> = HashMap::new();
        let sdk_req = VersionReq::parse(&format!("={}", metadata.sdk.version))
            .expect("`=<semver>` is always a valid VersionReq");
        deps.insert(metadata.sdk.name.clone(), sdk_req);
        for kit in metadata.kit.iter() {
            let req = VersionReq::parse(&format!("={}", kit.version))
                .expect("`=<semver>` is always a valid VersionReq");
            deps.insert(kit.name.clone(), req);
        }
        Ok(Some(deps))
    }
}

impl KitImageVendor {
    /// Report whether `<name>@<version>` looks like a Bottlerocket kit
    /// image by probing for the [`KIT_METADATA_LABEL`] label on its
    /// platform-selected image config.
    ///
    /// Any failure along the fetch path (missing manifest, missing
    /// platform image, unreachable registry, unreadable config) is
    /// treated as "not a kit image" so `get_options` cleanly filters
    /// non-kit tags out rather than surfacing a hard error from an
    /// unrelated image. Errors are logged at `warn` for visibility.
    async fn has_kit_metadata(&self, name: &str, version: &Version) -> bool {
        // Each fetch is wrapped in `retry_rate_limited` so a transient
        // `TOOMANYREQUESTS` from ECR is retried with exponential backoff
        // instead of poisoning the filter with false negatives. Any
        // other failure (missing manifest, image, config) is a genuine
        // "not a kit" signal and is returned immediately.
        let (uri, index) = match self.fetch_index(name, version).await {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!(
                    subsystem = "vendor",
                    component = "kit-image",
                    %name,
                    %version,
                    error = %err,
                    "failed to fetch manifest index while checking for kit metadata; skipping",
                );
                return false;
            }
        };
        let image = match retry_rate_limited(|| index.fetch_image(&uri, None)).await {
            Ok(Some(image)) => image,
            Ok(None) => return false,
            Err(err) => {
                tracing::warn!(
                    subsystem = "vendor",
                    component = "kit-image",
                    %name,
                    %version,
                    error = %err,
                    "failed to fetch platform image while checking for kit metadata; skipping",
                );
                return false;
            }
        };
        let config = match retry_rate_limited(|| image.fetch_config(&uri)).await {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!(
                    subsystem = "vendor",
                    component = "kit-image",
                    %name,
                    %version,
                    error = %err,
                    "failed to fetch image config while checking for kit metadata; skipping",
                );
                return false;
            }
        };
        config.config.labels.contains_key(KIT_METADATA_LABEL)
    }

    /// Fetch the manifest index for `<name>@<version>`. Tries the bare
    /// version tag first and falls back to a `v`-prefixed tag, matching
    /// edo-core's `ImageVendor::resolve` behaviour. Rate-limit responses
    /// from the registry are retried with exponential backoff so a
    /// concurrent discovery fanout doesn't produce false negatives.
    async fn fetch_index(&self, name: &str, version: &Version) -> SourceResult<(Uri, Index)> {
        let bare = Uri::builder()
            .registry(self.registry.clone())
            .repository(name)
            .reference(Reference::Tag(version.to_string()))
            .build();

        if let Ok(index) = retry_rate_limited(|| Index::fetch(&bare)).await {
            return Ok((bare, index));
        }
        let prefixed = Uri::builder()
            .registry(self.registry.clone())
            .repository(name)
            .reference(Reference::Tag(format!("v{version}")))
            .build();
        let index = retry_rate_limited(|| Index::fetch(&prefixed))
            .await
            .context(error::OciSnafu)
            .ok();
        ensure!(
            index.is_some(),
            error::VendedSnafu {
                name: name.to_string(),
                version: version.clone(),
            }
        );
        Ok((prefixed, index.unwrap()))
    }
}

/// Return `true` if `err` is a rate-limit response from the OCI
/// registry, i.e. any of its `errors[]` carry
/// `ErrorCode::TooManyRequests`.
///
/// Different fetch entry points wrap the response in different variants
/// (`FetchIndex`, `FetchManifest`, `FetchBlob`, `ListTags`, `ListRepos`),
/// so we match by structure rather than by variant to stay stable as
/// ocilot grows.
fn is_rate_limited(err: &ocilot::error::Error) -> bool {
    let response: Option<&ErrorResponse> = match err {
        ocilot::error::Error::FetchIndex { reason }
        | ocilot::error::Error::FetchManifest { reason }
        | ocilot::error::Error::FetchBlob { reason }
        | ocilot::error::Error::ListTags { reason }
        | ocilot::error::Error::ListRepos { reason } => Some(reason),
        _ => None,
    };
    response
        .map(|r| {
            r.errors
                .iter()
                .any(|e| matches!(e.code, ErrorCode::TooManyRequests))
        })
        .unwrap_or(false)
}

/// Retry `op` with exponential backoff while the OCI registry keeps
/// returning `TOOMANYREQUESTS`.
///
/// Non-rate-limit errors, and success, return immediately. After
/// [`RATE_LIMIT_RETRIES`] rate-limit responses the last error is
/// surfaced to the caller unchanged.
async fn retry_rate_limited<F, Fut, T>(mut op: F) -> ocilot::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ocilot::Result<T>>,
{
    let mut delay = RATE_LIMIT_INITIAL_DELAY;
    for attempt in 0..=RATE_LIMIT_RETRIES {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                if attempt < RATE_LIMIT_RETRIES && is_rate_limited(&err) {
                    tracing::debug!(
                        subsystem = "vendor",
                        component = "kit-image",
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        "registry returned TOOMANYREQUESTS; backing off",
                    );
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay.saturating_mul(2), RATE_LIMIT_MAX_DELAY);
                    continue;
                }
                return Err(err);
            }
        }
    }
    unreachable!("retry_rate_limited loop exited without returning")
}

pub mod error {
    use semver::Version;
    use snafu::Snafu;

    use edo::{
        context::{Addr, ContextError},
        source::SourceError,
    };

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(transparent)]
        Context {
            #[snafu(source(from(ContextError, Box::new)))]
            source: Box<ContextError>,
        },
        #[snafu(display("invalid kit-image vendor at {addr}: {source}"))]
        Invalid {
            addr: Addr,
            source: serde_json::Error,
        },
        #[snafu(display("io error occured interacting with oci registry: {source}"))]
        Io { source: std::io::Error },
        #[snafu(display("failed to interact with oci registry: {source}"))]
        Oci { source: ocilot::error::Error },
        #[snafu(display("could not find a kit image matching {name}@{version}"))]
        Vended { name: String, version: Version },
    }

    impl From<Error> for SourceError {
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
    use ocilot::models::{ErrorInfo, ErrorResponse};

    fn rate_limit_response() -> ErrorResponse {
        ErrorResponse {
            errors: vec![ErrorInfo {
                code: ErrorCode::TooManyRequests,
                message: Some("Rate exceeded".to_string()),
                detail: None,
            }],
        }
    }

    fn not_found_response() -> ErrorResponse {
        ErrorResponse {
            errors: vec![ErrorInfo {
                code: ErrorCode::ManifestUnknown,
                message: None,
                detail: None,
            }],
        }
    }

    #[test]
    fn is_rate_limited_detects_manifest_rate_limit() {
        let err = ocilot::error::Error::FetchManifest {
            reason: rate_limit_response(),
        };
        assert!(is_rate_limited(&err));
    }

    #[test]
    fn is_rate_limited_detects_index_rate_limit() {
        let err = ocilot::error::Error::FetchIndex {
            reason: rate_limit_response(),
        };
        assert!(is_rate_limited(&err));
    }

    #[test]
    fn is_rate_limited_ignores_other_error_codes() {
        let err = ocilot::error::Error::FetchManifest {
            reason: not_found_response(),
        };
        assert!(!is_rate_limited(&err));
    }

    #[test]
    fn is_rate_limited_ignores_unrelated_error_variants() {
        // A non-response-carrying variant must not be misclassified as
        // rate-limited even though the underlying transport may have
        // been overloaded.
        let err = ocilot::error::Error::ContentLengthMissing;
        assert!(!is_rate_limited(&err));
    }

    /// `retry_rate_limited` must retry while `TOOMANYREQUESTS` responses
    /// keep coming, and stop as soon as a non-rate-limit outcome (success
    /// or a different error) is produced.
    #[tokio::test]
    async fn retry_rate_limited_retries_then_succeeds() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        let result: ocilot::Result<u32> = retry_rate_limited(|| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    Err(ocilot::error::Error::FetchManifest {
                        reason: ErrorResponse {
                            errors: vec![ErrorInfo {
                                code: ErrorCode::TooManyRequests,
                                message: None,
                                detail: None,
                            }],
                        },
                    })
                } else {
                    Ok(42)
                }
            }
        })
        .await;
        assert_eq!(result.ok(), Some(42));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// Non-rate-limit errors must NOT trigger a retry; the caller sees
    /// them on the first attempt.
    #[tokio::test]
    async fn retry_rate_limited_bubbles_non_rate_limit_immediately() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        let result: ocilot::Result<u32> = retry_rate_limited(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move {
                Err(ocilot::error::Error::FetchManifest {
                    reason: not_found_response(),
                })
            }
        })
        .await;
        assert!(matches!(
            result,
            Err(ocilot::error::Error::FetchManifest { .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
