//! Metadata RPM spec construction and rpmbuild trigger
//!
//! Consumes the bundled `metadata.spec` along with the resolved [`ImageFeatures`]
//! to render the `bconds` conditions we utilize in selecting rpms.
use bon::Builder;
use edo::environment::{EnvironmentError, Vfs};

use crate::{
    model::Arch,
    model::{DEFAULT_VARIANT_FLAVOR, ImageFeatures, Variant},
};

const METADATA_SPEC: &str = include_str!("../../../data/metadata.spec");

#[derive(Clone, Builder)]
pub struct BuildMetadata {
    pub variant: Variant,
    pub arch: Arch,
    pub features: ImageFeatures,
    pub workdir: Vfs,
    pub output: Vfs,
}

impl BuildMetadata {
    pub async fn build(&self) -> Result<(), EnvironmentError> {
        // (1) Setup a rpmbuild environment for the metadata rpm
        let rpmbuild = self.workdir.create_dir("rpmbuild").await?;
        let build = rpmbuild.create_dir("BUILD").await?;
        let _ = rpmbuild.create_dir("BUILDROOT").await?;
        let rpms = rpmbuild.create_dir("RPMS").await?;
        let _ = rpmbuild.create_dir("SOURCES").await?;
        let specs = rpmbuild.create_dir("SPECS").await?;
        let _ = rpmbuild.create_dir("SRPMS").await?;

        // We need a `BUILD/sources` to standin for sources
        let _ = build.create_dir("sources").await?;

        // (2) SDK provided arch-specific rpm macros
        self.workdir
            .copy(
                format!("/usr/lib/rpm/platform/{}-bottlerocket/macros", self.arch),
                self.workdir.entry(".rpmmacros").await,
            )
            .await?;
        self.workdir
            .copy(
                format!("/usr/lib/rpm/platform/{}-bottlerocket/macros", self.arch),
                "/root/.rpmmacros",
            )
            .await?;

        // (3) Write the spec and preprend it with `%bcond_*` declarations that
        // encode the image-features set
        let bconds = self.features.to_bconds();
        let mut spec_bytes = bconds.into_bytes();
        spec_bytes.extend_from_slice(METADATA_SPEC.as_bytes());
        let spec_path = specs.entry("metadata.spec").await;
        specs.write(spec_path.clone(), &spec_bytes).await?;

        // (4) rpmbuild the metadata spec
        self.workdir
            .command(
                "build_metadata_rpm",
                "rpmbuild",
                &[
                    "--bb",
                    "--clean",
                    "--undefine _auto_set_build_flags",
                    &format!("--define \"_topdir {:?}\"", rpmbuild.path()),
                    &format!("--define \"_target_cpu {}\"", self.arch),
                    &format!("--define \"_cross_variant {}\"", self.variant),
                    &format!(
                        "--define \"_cross_variant_platform {}\"",
                        self.variant.platform()
                    ),
                    &format!(
                        "--define \"_cross_variant_runtime {}\"",
                        self.variant.runtime()
                    ),
                    &format!(
                        "--define \"_cross_variant_family {}\"",
                        self.variant.family()
                    ),
                    &format!(
                        "--define \"_cross_variant_flavor {}\"",
                        self.variant
                            .variant_flavor()
                            .unwrap_or(DEFAULT_VARIANT_FLAVOR)
                    ),
                    spec_path.as_ref(),
                ],
            )
            .await?;

        // (5) Copy the built RPMS to `output`.
        //
        // We can't use `Vfs::copy` here because it debug-formats both paths
        // (`cp -r "{from}" "{to}"`), which double-quotes the source and
        // suppresses shell glob expansion of `*.rpm`. `Vfs::command` joins
        // args unquoted and hands the whole line to `sh -c` inside the
        // environment, so the glob expands there.
        rpms.copy(
            self.arch.to_string(),
            self.output.entry("Packages/metadata").await,
        )
        .await?;

        Ok(())
    }
}
