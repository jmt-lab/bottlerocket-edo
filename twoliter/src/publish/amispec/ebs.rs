use aws_sdk_ec2::types::{
    EbsBlockDevice as SdkEbsBlockDevice, VolumeType as SdkVolumeType,
    builders::EbsBlockDeviceBuilder as SdkEbsBlockDeviceBuilder,
};
use bounded_integer::BoundedU64;
use num_traits::AsPrimitive;
use serde::{Deserialize, Serialize};

#[derive(Default, Debug, Clone, Serialize, Deserialize, Eq, PartialEq, bon::Builder)]
#[builder(on(_, into))]
pub struct EbsBlockDevice {
    #[serde(flatten)]
    pub volume_type: Volume,
    pub delete_on_termination: Option<bool>,
    pub encrypted: Option<bool>,
    pub kms_key_id: Option<String>,
    pub outpost_arn: Option<String>,
    pub snapshot_id: Option<String>,
}

impl EbsBlockDevice {
    pub fn create_sdk_ebs_block_device(&self) -> SdkEbsBlockDevice {
        let builder = SdkEbsBlockDeviceBuilder::default()
            .set_delete_on_termination(self.delete_on_termination)
            .set_encrypted(self.encrypted)
            .set_kms_key_id(self.kms_key_id.clone())
            .set_outpost_arn(self.outpost_arn.clone())
            .set_snapshot_id(self.snapshot_id.clone());
        let builder = self.volume_type.build(builder);
        builder.build()
    }

    pub fn volume_size(&self) -> Option<u64> {
        match self.volume_type {
            Volume::Standard { volume_size } => volume_size.map(|x| x.as_()),
            Volume::Io1 { volume_size, .. } => volume_size.map(|x| x.as_()),
            Volume::Io2 { volume_size, .. } => volume_size.map(|x| x.as_()),
            Volume::Gp2 { volume_size } => volume_size.map(|x| x.as_()),
            Volume::Gp3 { volume_size, .. } => volume_size.map(|x| x.as_()),
            Volume::Sc1 { volume_size } => volume_size.map(|x| x.as_()),
            Volume::St1 { volume_size } => volume_size.map(|x| x.as_()),
        }
    }

    pub fn set_volume_size(&mut self, value: u64) -> Option<u64> {
        match self.volume_type {
            Volume::Standard { mut volume_size } => volume_size = BoundedU64::new(value),
            Volume::Io1 {
                mut volume_size, ..
            } => volume_size = BoundedU64::new(value),
            Volume::Io2 {
                mut volume_size, ..
            } => volume_size = BoundedU64::new(value),
            Volume::Gp2 {
                mut volume_size, ..
            } => volume_size = BoundedU64::new(value),
            Volume::Gp3 {
                mut volume_size, ..
            } => volume_size = BoundedU64::new(value),
            Volume::Sc1 { mut volume_size } => volume_size = BoundedU64::new(value),
            Volume::St1 { mut volume_size } => volume_size = BoundedU64::new(value),
        }
        self.volume_size()
    }

    pub fn iops(&self) -> Option<u64> {
        match self.volume_type {
            Volume::Io1 { iops, .. } => Some(iops.as_()),
            Volume::Io2 { iops, .. } => Some(iops.as_()),
            Volume::Gp3 { iops, .. } => iops.map(|x| x.as_()),
            _ => None,
        }
    }

    pub fn throughput(&self) -> Option<u64> {
        match self.volume_type {
            Volume::Gp3 { throughput, .. } => throughput.map(|x| x.as_()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", tag = "volume-type")]
pub enum Volume {
    Standard {
        volume_size: Option<BoundedU64<1, 1_024>>,
    },
    Io1 {
        volume_size: Option<BoundedU64<4, 16_384>>,
        iops: BoundedU64<100, 64_000>,
    },
    Io2 {
        volume_size: Option<BoundedU64<4, 16_384>>,
        iops: BoundedU64<100, 64_000>,
    },
    Gp2 {
        volume_size: Option<BoundedU64<1, 16_384>>,
    },
    Gp3 {
        volume_size: Option<BoundedU64<1, 16_384>>,
        iops: Option<BoundedU64<3_000, 16_000>>,
        throughput: Option<BoundedU64<125, 1_000>>,
    },
    Sc1 {
        volume_size: Option<BoundedU64<125, 16_384>>,
    },
    St1 {
        volume_size: Option<BoundedU64<125, 16_384>>,
    },
}

impl Default for Volume {
    fn default() -> Self {
        Self::Standard { volume_size: None }
    }
}

impl Volume {
    fn build(&self, builder: SdkEbsBlockDeviceBuilder) -> SdkEbsBlockDeviceBuilder {
        match self {
            Self::Standard { volume_size } => builder
                .volume_type(SdkVolumeType::Standard)
                .set_volume_size(volume_size.map(|x| x.as_())),
            Self::Io1 { volume_size, iops } => builder
                .volume_type(SdkVolumeType::Io1)
                .set_volume_size(volume_size.map(|x| x.as_()))
                .iops(iops.as_()),
            Self::Io2 { volume_size, iops } => builder
                .volume_type(SdkVolumeType::Io2)
                .set_volume_size(volume_size.map(|x| x.as_()))
                .iops(iops.as_()),
            Self::Gp2 { volume_size } => builder
                .volume_type(SdkVolumeType::Gp2)
                .set_volume_size(volume_size.map(|x| x.as_())),
            Self::Gp3 {
                volume_size,
                iops,
                throughput,
            } => builder
                .volume_type(SdkVolumeType::Gp3)
                .set_volume_size(volume_size.map(|x| x.as_()))
                .set_iops(iops.map(|x| x.as_()))
                .set_throughput(throughput.map(|x| x.as_())),
            Self::Sc1 { volume_size } => builder
                .volume_type(SdkVolumeType::Sc1)
                .set_volume_size(volume_size.map(|x| x.as_())),
            Self::St1 { volume_size } => builder
                .volume_type(SdkVolumeType::St1)
                .set_volume_size(volume_size.map(|x| x.as_())),
        }
    }
}
