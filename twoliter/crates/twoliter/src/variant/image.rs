use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Eif,
    Qcow2,
    #[default]
    Raw,
    Vmdk,
}

impl std::fmt::Display for ImageFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Eif => "eif",
            Self::Qcow2 => "qcow2",
            Self::Raw => "raw",
            Self::Vmdk => "vmdk",
        })
    }
}

/// Disk partitioning strategy.
#[derive(Deserialize, Serialize, Debug, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PartitionPlan {
    Split,
    Unified,
}

impl std::fmt::Display for PartitionPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Split => "split",
            Self::Unified => "unified",
        })
    }
}
