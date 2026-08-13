//! Check whether an AMI's launch permissions include the `all`
//! group (i.e. the AMI is public).
//!
//! Ported verbatim from
//! `old-twoliter/tools/pubsys/src/aws/ami/public.rs`.
use aws_sdk_ec2::{
    Client as Ec2Client,
    types::{ImageAttributeName, PermissionGroup},
};
use error_utils::AwsSdkError;
use snafu::ResultExt;

/// True iff `ami_id` has the `all` group in its launch permissions.
pub(crate) async fn ami_is_public(
    ec2_client: &Ec2Client,
    region: &str,
    ami_id: &str,
) -> Result<bool> {
    let response = ec2_client
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

    let is_public = response.launch_permissions().iter().any(|lp| {
        lp.group
            .as_ref()
            .map(|g| g == &PermissionGroup::All)
            .unwrap_or(false)
    });
    Ok(is_public)
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
    }
}
pub(crate) use error::Error;
type Result<T> = std::result::Result<T, error::Error>;
