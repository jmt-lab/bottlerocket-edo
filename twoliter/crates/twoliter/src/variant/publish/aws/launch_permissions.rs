//! Fetch AMI launch permissions (`user_ids`, group names, org ARNs,
//! OU ARNs) and represent them as a strongly-typed enum for JSON
//! output.
//!
//! Ported verbatim from
//! `old-twoliter/tools/pubsys/src/aws/ami/launch_permissions.rs`.
use aws_sdk_ec2::{
    Client as Ec2Client,
    types::{ImageAttributeName, LaunchPermission},
};
use error_utils::AwsSdkError;
use serde::{Deserialize, Serialize};
use snafu::ResultExt;

/// Return every launch permission set on `ami_id`.
pub(crate) async fn get_launch_permissions(
    ec2_client: &Ec2Client,
    region: &str,
    ami_id: &str,
) -> Result<Vec<LaunchPermissionDef>> {
    let ec2_response = ec2_client
        .describe_image_attribute()
        .image_id(ami_id)
        .attribute(ImageAttributeName::LaunchPermission)
        .send()
        .await
        .map_err(AwsSdkError::from)
        .context(error::DescribeImageAttributeSnafu {
            ami_id,
            region: region.to_string(),
        })?;

    let mut launch_permissions = vec![];
    for permission in ec2_response.launch_permissions().to_vec() {
        launch_permissions.push(LaunchPermissionDef::try_from(permission)?);
    }
    Ok(launch_permissions)
}

/// Kind-tagged launch-permission entry used both in the AWS-side
/// grant/revoke input and in the JSON output that describes the
/// registered AMI.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, Hash)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LaunchPermissionDef {
    Group(String),
    UserId(String),
    OrganizationArn(String),
    OrganizationalUnitArn(String),
}

impl TryFrom<LaunchPermission> for LaunchPermissionDef {
    type Error = Error;

    fn try_from(lp: LaunchPermission) -> Result<Self> {
        let LaunchPermission {
            group,
            user_id,
            organization_arn,
            organizational_unit_arn,
            ..
        } = lp;

        // Exactly one of the four fields is expected to be set.
        // Match the old-twoliter behavior: return the first one that's
        // present, using the enum's declaration order as tiebreaker.
        if let Some(g) = group {
            Ok(Self::Group(g.as_str().to_string()))
        } else if let Some(u) = user_id {
            Ok(Self::UserId(u))
        } else if let Some(o) = organization_arn {
            Ok(Self::OrganizationArn(o))
        } else if let Some(ou) = organizational_unit_arn {
            Ok(Self::OrganizationalUnitArn(ou))
        } else {
            error::EmptyLaunchPermissionSnafu.fail()
        }
    }
}

pub(crate) mod error {
    use aws_sdk_ec2::operation::describe_image_attribute::DescribeImageAttributeError;
    use error_utils::AwsSdkError;
    use snafu::Snafu;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(crate)))]
    #[allow(clippy::large_enum_variant)]
    pub enum Error {
        #[snafu(display(
            "Failed to describe image attributes for {ami_id} in {region}: {source}"
        ))]
        DescribeImageAttribute {
            ami_id: String,
            region: String,
            #[snafu(source(from(AwsSdkError<DescribeImageAttributeError>, Box::new)))]
            source: Box<AwsSdkError<DescribeImageAttributeError>>,
        },

        #[snafu(display("DescribeImageAttribute returned an empty launch permission entry"))]
        EmptyLaunchPermission,
    }
}
pub(crate) use error::Error;
type Result<T> = std::result::Result<T, error::Error>;
