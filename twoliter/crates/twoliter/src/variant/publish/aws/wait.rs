//! Poll `DescribeImages` until an AMI reaches a target state a
//! configurable number of times in a row (so a caller doesn't act on
//! a briefly-consistent read).
//!
//! Ported from `old-twoliter/tools/pubsys/src/aws/ami/wait.rs` with
//! two mechanical changes: `log::` → `tracing::` and
//! `std::thread::sleep` → `tokio::time::sleep`. The latter matters:
//! we're inside a tokio runtime, so a blocking sleep would starve
//! sibling tasks on the same worker thread.
use super::client::build_client_config;
use aws_sdk_ec2::{Client as Ec2Client, config::Region, types::ImageState};
use pubsys_config::AwsConfig as PubsysAwsConfig;
use snafu::{ResultExt, ensure};
use std::time::Duration;

/// Wait for `id` in `region` to reach `state`, requiring
/// `successes_required` consecutive reads reporting that state
/// before returning `Ok`.
pub(crate) async fn wait_for_ami(
    id: &str,
    region: &Region,
    sts_region: &Region,
    state: &str,
    successes_required: u8,
    pubsys_aws_config: &PubsysAwsConfig,
) -> Result<()> {
    let mut successes = 0u8;
    let max_attempts: u8 = 90;
    let mut attempts: u8 = 0;
    let seconds_between_attempts = 2;

    loop {
        attempts += 1;
        // Stop after max_attempts unless the caller is on a success
        // streak; a run of consistent reads earns a little wiggle
        // room.
        ensure!(
            attempts.saturating_sub(successes) <= max_attempts,
            error::MaxAttemptsSnafu {
                id,
                max_attempts,
                region: region.as_ref(),
            }
        );

        // Rebuild the client each iteration: gives the SDK an
        // opportunity to talk to a different endpoint IP and gives
        // us a defense against a single-endpoint stale-read cache.
        let client_config = build_client_config(region, sts_region, pubsys_aws_config).await;
        let ec2_client = Ec2Client::new(&client_config);
        let describe_response = ec2_client
            .describe_images()
            .set_image_ids(Some(vec![id.to_string()]))
            .send()
            .await
            .map_err(error_utils::AwsSdkError::from)
            .context(error::DescribeImagesSnafu {
                region: region.as_ref(),
            })?;

        if let Some(images) = describe_response.images {
            let mut saw_it = false;
            for image in images {
                if let Some(found_id) = image.image_id
                    && let Some(found_state) = image.state
                {
                    if id == found_id && ImageState::from(state) == found_state {
                        saw_it = true;
                        successes += 1;
                        if successes >= successes_required {
                            tracing::info!("Found {id} {state} in {region}");
                            return Ok(());
                        }
                        break;
                    }
                    // Terminal-failure states shortcut the loop.
                    match &found_state {
                        ImageState::Invalid
                        | ImageState::Deregistered
                        | ImageState::Failed
                        | ImageState::Error => {
                            error::StateSnafu {
                                id,
                                state: found_state.as_ref(),
                                region: region.as_ref(),
                            }
                            .fail()?
                        }
                        _ => {}
                    }
                }
            }
            if !saw_it {
                // Image wasn't in the returned list; reset streak.
                successes = 0;
            }
        } else {
            successes = 0;
        }

        if attempts % 5 == 1 {
            tracing::info!(
                "Waiting for {id} in {region} to be {state}... (attempt {attempts} of {max_attempts})"
            );
        }
        tokio::time::sleep(Duration::from_secs(seconds_between_attempts)).await;
    }
}

pub(crate) mod error {
    use aws_sdk_ec2::operation::describe_images::DescribeImagesError;
    use error_utils::AwsSdkError;
    use snafu::Snafu;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(crate)))]
    #[allow(clippy::large_enum_variant)]
    pub enum Error {
        #[snafu(display("Failed to describe images in {region}: {source}"))]
        DescribeImages {
            region: String,
            #[snafu(source(from(AwsSdkError<DescribeImagesError>, Box::new)))]
            source: Box<AwsSdkError<DescribeImagesError>>,
        },

        #[snafu(display(
            "Failed to reach desired state within {max_attempts} attempts for {id} in {region}"
        ))]
        MaxAttempts {
            max_attempts: u8,
            id: String,
            region: String,
        },

        #[snafu(display("Image '{id}' went to '{state}' state in {region}"))]
        State {
            id: String,
            state: String,
            region: String,
        },
    }
}
pub(crate) use error::Error;
type Result<T> = std::result::Result<T, error::Error>;
