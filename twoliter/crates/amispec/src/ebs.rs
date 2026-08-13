//! Serde-based (de-)serialization for EBS block devices used in an AMI
//! block-device mapping, with client-side validation of volume-size /
//! iops / throughput bounds per volume type.
//!
//! Ported (trimmed) from
//! `old-twoliter/tools/amispec/src/ebs.rs`. The old crate generated
//! twin `Templated*` structs via a proc-macro so that a
//! handlebars-templated on-disk amispec could render into a concrete
//! one at build time. We drop the templating layer entirely: the new
//! `mk_amispec` pipeline runs `handlebars::render_template` on the
//! *raw TOML text* before deserializing into these plain types (see
//! `crates/twoliter/src/variant/publish/register/mk_amispec.rs`), so
//! the user-facing `{{ foo }}` contract is preserved without the
//! proc-macro machinery.
use aws_sdk_ec2::types::{EbsBlockDevice as SdkEbsBlockDevice, VolumeType as SdkVolumeType};
use aws_sdk_ec2::types::builders::EbsBlockDeviceBuilder;
use bounded_integer::BoundedU64;
use num_traits::AsPrimitive;
use serde::{Deserialize, Serialize};
use snafu::{OptionExt, Snafu};

/// Discriminated union of EBS volume types the amispec understands.
///
/// The `volume-type` field selects the variant at deserialization
/// time (`#[serde(tag = "volume-type")]`). Each variant carries a
/// struct with its own bounded fields, so an out-of-range
/// `volume-size` (or `iops` or `throughput`) is rejected at parse
/// time rather than at `RegisterImage` time.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", tag = "volume-type")]
pub enum EbsBlockDevice {
    Standard(Standard),
    Io1(Io1),
    Io2(Io2),
    Gp2(Gp2),
    Sc1(Sc1),
    St1(St1),
    Gp3(Gp3),
}

impl EbsBlockDevice {
    /// Build the AWS SDK `EbsBlockDevice` payload for the
    /// `RegisterImage` call.
    pub(crate) fn create_sdk_ebs_block_device(&self) -> SdkEbsBlockDevice {
        let builder = EbsBlockDeviceBuilder::default();
        match self {
            Self::Standard(d) => d.configure(builder).volume_type(SdkVolumeType::Standard),
            Self::Io1(d) => d.configure(builder).volume_type(SdkVolumeType::Io1),
            Self::Io2(d) => d.configure(builder).volume_type(SdkVolumeType::Io2),
            Self::Gp2(d) => d.configure(builder).volume_type(SdkVolumeType::Gp2),
            Self::Sc1(d) => d.configure(builder).volume_type(SdkVolumeType::Sc1),
            Self::St1(d) => d.configure(builder).volume_type(SdkVolumeType::St1),
            Self::Gp3(d) => d.configure(builder).volume_type(SdkVolumeType::Gp3),
        }
        .build()
    }

    pub fn as_ebs_device(&self) -> &dyn EbsBlockDeviceKind {
        match self {
            Self::Standard(d) => d,
            Self::Io1(d) => d,
            Self::Io2(d) => d,
            Self::Gp2(d) => d,
            Self::Sc1(d) => d,
            Self::St1(d) => d,
            Self::Gp3(d) => d,
        }
    }

    pub fn as_mut_ebs_device(&mut self) -> &mut dyn EbsBlockDeviceKind {
        match self {
            Self::Standard(d) => d,
            Self::Io1(d) => d,
            Self::Io2(d) => d,
            Self::Gp2(d) => d,
            Self::Sc1(d) => d,
            Self::St1(d) => d,
            Self::Gp3(d) => d,
        }
    }
}

/// Attributes common to every EBS volume, factored out to keep each
/// concrete volume-type struct focused on the type-specific bounds.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct EbsCommon {
    pub delete_on_termination: Option<bool>,
    pub encrypted: Option<bool>,
    pub kms_key_id: Option<String>,
    pub outpost_arn: Option<String>,
    pub snapshot_id: Option<String>,
}

/// Apply the shared `EbsCommon` fields onto an SDK builder. Extracted
/// so every concrete volume-type struct's `configure()` implementation
/// can lean on it.
fn configure_common(mut b: EbsBlockDeviceBuilder, c: &EbsCommon) -> EbsBlockDeviceBuilder {
    b = b
        .set_delete_on_termination(c.delete_on_termination)
        .set_encrypted(c.encrypted)
        .set_kms_key_id(c.kms_key_id.clone())
        .set_outpost_arn(c.outpost_arn.clone())
        .set_snapshot_id(c.snapshot_id.clone());
    b
}

// -----------------------------------------------------------------------------
// Concrete volume types. Bounds match `old-twoliter/tools/amispec/src/ebs.rs`.
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Standard {
    pub volume_size: Option<BoundedU64<1, 1_024>>,
    #[serde(flatten)]
    pub common: EbsCommon,
}

impl Standard {
    fn configure(&self, builder: EbsBlockDeviceBuilder) -> EbsBlockDeviceBuilder {
        configure_common(builder, &self.common).set_volume_size(self.volume_size.map(AsPrimitive::as_))
    }
    pub fn volume_type() -> &'static str {
        "standard"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Io1 {
    pub volume_size: Option<BoundedU64<4, 16_384>>,
    pub iops: BoundedU64<100, 64_000>,
    #[serde(flatten)]
    pub common: EbsCommon,
}

impl Io1 {
    fn configure(&self, builder: EbsBlockDeviceBuilder) -> EbsBlockDeviceBuilder {
        configure_common(builder, &self.common)
            .set_volume_size(self.volume_size.map(AsPrimitive::as_))
            .iops(self.iops.as_())
    }
    pub fn volume_type() -> &'static str {
        "io1"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Io2 {
    pub volume_size: Option<BoundedU64<4, 65_536>>,
    pub iops: BoundedU64<100, 256_000>,
    #[serde(flatten)]
    pub common: EbsCommon,
}

impl Io2 {
    fn configure(&self, builder: EbsBlockDeviceBuilder) -> EbsBlockDeviceBuilder {
        configure_common(builder, &self.common)
            .set_volume_size(self.volume_size.map(AsPrimitive::as_))
            .iops(self.iops.as_())
    }
    pub fn volume_type() -> &'static str {
        "io2"
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Gp2 {
    pub volume_size: Option<BoundedU64<1, 16_384>>,
    #[serde(flatten)]
    pub common: EbsCommon,
}

impl Gp2 {
    fn configure(&self, builder: EbsBlockDeviceBuilder) -> EbsBlockDeviceBuilder {
        configure_common(builder, &self.common).set_volume_size(self.volume_size.map(AsPrimitive::as_))
    }
    pub fn volume_type() -> &'static str {
        "gp2"
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Sc1 {
    pub volume_size: Option<BoundedU64<125, 16_384>>,
    #[serde(flatten)]
    pub common: EbsCommon,
}

impl Sc1 {
    fn configure(&self, builder: EbsBlockDeviceBuilder) -> EbsBlockDeviceBuilder {
        configure_common(builder, &self.common).set_volume_size(self.volume_size.map(AsPrimitive::as_))
    }
    pub fn volume_type() -> &'static str {
        "sc1"
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct St1 {
    pub volume_size: Option<BoundedU64<125, 16_384>>,
    #[serde(flatten)]
    pub common: EbsCommon,
}

impl St1 {
    fn configure(&self, builder: EbsBlockDeviceBuilder) -> EbsBlockDeviceBuilder {
        configure_common(builder, &self.common).set_volume_size(self.volume_size.map(AsPrimitive::as_))
    }
    pub fn volume_type() -> &'static str {
        "st1"
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Gp3 {
    pub volume_size: Option<BoundedU64<1, 16_384>>,
    pub iops: Option<BoundedU64<3_000, 16_000>>,
    pub throughput: Option<BoundedU64<125, 1_000>>,
    #[serde(flatten)]
    pub common: EbsCommon,
}

impl Gp3 {
    fn configure(&self, builder: EbsBlockDeviceBuilder) -> EbsBlockDeviceBuilder {
        configure_common(builder, &self.common)
            .set_iops(self.iops.map(AsPrimitive::as_))
            .set_throughput(self.throughput.map(AsPrimitive::as_))
            .set_volume_size(self.volume_size.map(AsPrimitive::as_))
    }
    pub fn volume_type() -> &'static str {
        "gp3"
    }
}

// -----------------------------------------------------------------------------
// Common trait for read/write access to shared EBS attributes without
// dispatching on the concrete volume-type variant at call sites.
// -----------------------------------------------------------------------------

pub trait EbsBlockDeviceKind {
    fn volume_type(&self) -> &str;

    fn volume_size(&self) -> Option<u64>;
    fn set_volume_size(&mut self, volume_size: Option<u64>) -> Result<(), InvalidValueError>;

    fn delete_on_termination(&self) -> Option<bool>;
    fn set_delete_on_termination(&mut self, delete_on_termination: Option<bool>);

    fn encrypted(&self) -> Option<bool>;
    fn set_encrypted(&mut self, encrypted: Option<bool>);

    fn kms_key_id(&self) -> Option<&str>;
    fn set_kms_key_id(&mut self, kms_key_id: Option<String>);

    fn outpost_arn(&self) -> Option<&str>;
    fn set_outpost_arn(&mut self, outpost_arn: Option<String>);

    fn snapshot_id(&self) -> Option<&str>;
    fn set_snapshot_id(&mut self, snapshot_id: Option<String>);
}

macro_rules! impl_ebs_kind {
    ($ty:ty) => {
        impl EbsBlockDeviceKind for $ty {
            fn volume_type(&self) -> &str {
                Self::volume_type()
            }
            fn volume_size(&self) -> Option<u64> {
                self.volume_size.map(AsPrimitive::as_)
            }
            fn set_volume_size(&mut self, volume_size: Option<u64>) -> Result<(), InvalidValueError> {
                if let Some(v) = volume_size {
                    self.volume_size = Some(bounded_from_u64(v, Self::volume_type(), "volume-size")?);
                }
                Ok(())
            }
            fn delete_on_termination(&self) -> Option<bool> {
                self.common.delete_on_termination
            }
            fn set_delete_on_termination(&mut self, delete_on_termination: Option<bool>) {
                self.common.delete_on_termination = delete_on_termination;
            }
            fn encrypted(&self) -> Option<bool> {
                self.common.encrypted
            }
            fn set_encrypted(&mut self, encrypted: Option<bool>) {
                self.common.encrypted = encrypted;
            }
            fn kms_key_id(&self) -> Option<&str> {
                self.common.kms_key_id.as_deref()
            }
            fn set_kms_key_id(&mut self, kms_key_id: Option<String>) {
                self.common.kms_key_id = kms_key_id;
            }
            fn outpost_arn(&self) -> Option<&str> {
                self.common.outpost_arn.as_deref()
            }
            fn set_outpost_arn(&mut self, outpost_arn: Option<String>) {
                self.common.outpost_arn = outpost_arn;
            }
            fn snapshot_id(&self) -> Option<&str> {
                self.common.snapshot_id.as_deref()
            }
            fn set_snapshot_id(&mut self, snapshot_id: Option<String>) {
                self.common.snapshot_id = snapshot_id;
            }
        }
    };
}

impl_ebs_kind!(Standard);
impl_ebs_kind!(Io1);
impl_ebs_kind!(Io2);
impl_ebs_kind!(Gp2);
impl_ebs_kind!(Sc1);
impl_ebs_kind!(St1);
impl_ebs_kind!(Gp3);

impl EbsBlockDeviceKind for EbsBlockDevice {
    fn volume_type(&self) -> &str {
        self.as_ebs_device().volume_type()
    }
    fn volume_size(&self) -> Option<u64> {
        self.as_ebs_device().volume_size()
    }
    fn delete_on_termination(&self) -> Option<bool> {
        self.as_ebs_device().delete_on_termination()
    }
    fn encrypted(&self) -> Option<bool> {
        self.as_ebs_device().encrypted()
    }
    fn kms_key_id(&self) -> Option<&str> {
        self.as_ebs_device().kms_key_id()
    }
    fn outpost_arn(&self) -> Option<&str> {
        self.as_ebs_device().outpost_arn()
    }
    fn snapshot_id(&self) -> Option<&str> {
        self.as_ebs_device().snapshot_id()
    }

    fn set_volume_size(&mut self, volume_size: Option<u64>) -> Result<(), InvalidValueError> {
        self.as_mut_ebs_device().set_volume_size(volume_size)
    }
    fn set_delete_on_termination(&mut self, v: Option<bool>) {
        self.as_mut_ebs_device().set_delete_on_termination(v);
    }
    fn set_encrypted(&mut self, v: Option<bool>) {
        self.as_mut_ebs_device().set_encrypted(v);
    }
    fn set_kms_key_id(&mut self, v: Option<String>) {
        self.as_mut_ebs_device().set_kms_key_id(v);
    }
    fn set_outpost_arn(&mut self, v: Option<String>) {
        self.as_mut_ebs_device().set_outpost_arn(v);
    }
    fn set_snapshot_id(&mut self, v: Option<String>) {
        self.as_mut_ebs_device().set_snapshot_id(v);
    }
}

/// Fallible constructor for the bounded-integer size fields. Bounds
/// are compile-time consts; the value is a runtime u64, so `BoundedU64`
/// returns `None` on out-of-range. Convert that to a snafu error that
/// names the volume-type + field for user diagnostics.
fn bounded_from_u64<const MIN: u64, const MAX: u64>(
    value: u64,
    volume_type: &str,
    field: &str,
) -> Result<BoundedU64<MIN, MAX>, InvalidValueError> {
    BoundedU64::new(value).context(invalid_value_error::InvalidValueSnafu {
        min: MIN,
        max: MAX,
        field,
        volume_type,
        value,
    })
}

#[derive(Debug, Snafu)]
#[snafu(module)]
#[snafu(display(
    "Invalid value '{value}' not within bounds [{min}, {max}] for field '{field}' of volume-type '{volume_type}'"
))]
pub struct InvalidValueError {
    value: u64,
    min: u64,
    max: u64,
    field: String,
    volume_type: String,
}
