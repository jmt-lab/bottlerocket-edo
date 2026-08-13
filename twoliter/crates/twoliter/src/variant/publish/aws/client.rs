//! AWS client builder: layers profile → base role → regional role
//! credentials providers on top of the environment defaults.
//!
//! Ported verbatim from `old-twoliter/tools/pubsys/src/aws/client.rs`.
use aws_config::SdkConfig;
use aws_config::default_provider::credentials::default_provider;
use aws_config::profile::ProfileFileCredentialsProvider;
use aws_config::sts::AssumeRoleProvider;
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_types::region::Region;
use pubsys_config::AwsConfig as PubsysAwsConfig;

/// Build an SDK config for `region` using credentials configured in
/// `pubsys_aws_config`. `sts_region` is the region the AssumeRole
/// calls speak to (may differ from `region` when the base account
/// has not opted into the target region).
pub(crate) async fn build_client_config(
    region: &Region,
    sts_region: &Region,
    pubsys_aws_config: &PubsysAwsConfig,
) -> SdkConfig {
    let maybe_profile = pubsys_aws_config.profile.clone();
    let maybe_role = pubsys_aws_config.role.clone();
    let maybe_regional_role = pubsys_aws_config
        .region
        .get(region.as_ref())
        .and_then(|r| r.role.clone());
    let base_provider = base_provider(&maybe_profile).await;

    let config = match (&maybe_role, &maybe_regional_role) {
        (None, None) => aws_config::from_env().credentials_provider(base_provider),
        _ => {
            let assume_roles = maybe_role.iter().chain(maybe_regional_role.iter()).cloned();
            let provider =
                build_provider(sts_region, assume_roles.clone(), base_provider.clone()).await;
            aws_config::from_env().credentials_provider(provider)
        }
    };

    config.region(region.clone()).load().await
}

/// Chain credentials providers to assume the given roles in order,
/// speaking to STS in `sts_region`.
async fn build_provider(
    sts_region: &Region,
    assume_roles: impl Iterator<Item = String>,
    base_provider: SharedCredentialsProvider,
) -> SharedCredentialsProvider {
    let mut provider = base_provider;
    for assume_role in assume_roles {
        provider = SharedCredentialsProvider::new(
            AssumeRoleProvider::builder(assume_role)
                .region(sts_region.clone())
                .session_name("twoliter-publish-ami")
                .build_from_provider(provider.clone())
                .await,
        );
    }
    provider
}

/// Use the named profile if one is set, otherwise fall back to the
/// SDK's default credentials-provider chain.
async fn base_provider(maybe_profile: &Option<String>) -> SharedCredentialsProvider {
    if let Some(profile) = maybe_profile {
        SharedCredentialsProvider::new(
            ProfileFileCredentialsProvider::builder()
                .profile_name(profile)
                .build(),
        )
    } else {
        SharedCredentialsProvider::new(default_provider().await)
    }
}
