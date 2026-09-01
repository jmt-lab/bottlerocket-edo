use serde::{Deserialize, Serialize};
use snafu::Snafu;

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    #[serde(alias = "amd64")]
    X86_64,
    #[serde(alias = "arm64")]
    Aarch64,
}

impl Arch {
    pub fn parse<S: AsRef<str>>(value: S) -> Result<Self, ArchError> {
        let lower = value.as_ref().to_lowercase();
        match lower.as_str() {
            "x86_64" | "amd64" => Ok(Self::X86_64),
            "aarch64" | "arm64" => Ok(Self::Aarch64),
            value => UnsupportedSnafu { value }.fail(),
        }
    }
}

impl Default for Arch {
    fn default() -> Self {
        match std::env::consts::ARCH {
            "x86_64" => Self::X86_64,
            _ => Self::Aarch64,
        }
    }
}

impl Arch {
    pub fn goarch(&self) -> &'static str {
        match self {
            Arch::X86_64 => "amd64",
            Arch::Aarch64 => "arm64",
        }
    }
}

impl std::fmt::Display for Arch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::X86_64 => f.write_str("x86_64"),
            Self::Aarch64 => f.write_str("aarch64"),
        }
    }
}

impl std::str::FromStr for Arch {
    type Err = ArchError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[derive(Snafu, Debug)]
pub enum ArchError {
    #[snafu(display("unsupported architecture: {value}"))]
    Unsupported { value: String },
}
