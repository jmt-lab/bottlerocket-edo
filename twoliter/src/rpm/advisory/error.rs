use edo::context::ContextError;
use edo::transform::TransformError;
use snafu::Snafu;
use std::path::PathBuf;
use std::process::Output;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("Failed to execute command: {}", source))]
    ExecutionFailure { source: std::io::Error },
    #[snafu(transparent)]
    Context { source: edo::context::ContextError },
    #[snafu(transparent)]
    Environment {
        source: edo::environment::EnvironmentError,
    },
    #[snafu(transparent)]
    Storage { source: edo::storage::StorageError },
    #[snafu(transparent)]
    Transform {
        source: edo::transform::TransformError,
    },
    #[snafu(display("No `packages` or `advisories` folder"))]
    InvalidSetup,

    #[snafu(display("Command '{}' failed: {}", bin_path, String::from_utf8_lossy(&output.stderr)))]
    CommandFailure { bin_path: String, output: Output },

    #[snafu(display("Failed to read '{}'", path.display()))]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("Failed to parse advisory '{}'", path.display()))]
    ParseAdvisory {
        path: PathBuf,
        source: toml::de::Error,
    },

    #[snafu(display(
        "Incorrect output format for rpmspec command. Expected name|epoch|version. Got '{spec_output}'"
    ))]
    RpmSpecFormat { spec_output: String },

    #[snafu(display("Failed to read directory '{}'", path.display()))]
    ReadDir {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("Failed to read directory entry"))]
    ReadDirEntry { source: std::io::Error },

    #[snafu(display("{} Advisory violations found.", violations.len()))]
    AdvisoryViolations { violations: Vec<String> },

    #[snafu(display("No source provided to check for advisories"))]
    NoSource,
}

impl From<Error> for ContextError {
    fn from(value: Error) -> Self {
        Self::Component {
            source: Box::new(value),
        }
    }
}

impl From<Error> for TransformError {
    fn from(value: Error) -> Self {
        Self::Implementation {
            source: Box::new(value),
        }
    }
}
