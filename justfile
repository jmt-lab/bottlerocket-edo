github := "bottlerocket-os"

setup:
    cargo install tomlq

clone kit:
    git clone https://github.com/{{github}}/{{kit}}.git repos/{{kit}}

# Generate `variants/<variant>/variant.toml` files describing each
# bottlerocket variant. Each source `repos/bottlerocket/variants/<v>/Cargo.toml`
# is translated into an equivalent twoliter-next `variant.toml` at
# `variants/<v>/variant.toml`.
_variant-toml:
    #!/usr/bin/env bash
    set -euo pipefail
    source="repos/bottlerocket"
    version="$(tq --raw .version < "${source}/Release.toml")"

    for cargo_toml in "${source}"/variants/*/Cargo.toml; do
        variant_dir="$(dirname "${cargo_toml}")"
        variant="$(basename "${variant_dir}")"
        element="variants/${variant}"
        mkdir -p "${element}"

        echo "converting variant ${variant}"

        # Slurp the whole Cargo.toml as JSON for easy jq queries.
        cargo_json="$(tq -o json . < "${cargo_toml}")"
        bv="$(jq -c '.package.metadata["build-variant"] // {}' <<< "${cargo_json}")"

        # `image-format` and `partition-plan` have no schema defaults in
        # twoliter-next (`config/variant.rs::VariantImageBlock`), so we
        # fill in old-twoliter's implicit defaults (`raw` / `split`) when
        # the upstream Cargo.toml omits them.
        image_format="$(jq --raw-output '.["image-format"] // "raw"' <<< "${bv}")"
        partition_plan="$(jq --raw-output '.["image-layout"]["partition-plan"] // "split"' <<< "${bv}")"
        os_size_gib="$(jq --raw-output '.["image-layout"]["os-image-size-gib"] // empty' <<< "${bv}")"
        data_size_gib="$(jq --raw-output '.["image-layout"]["data-image-size-gib"] // empty' <<< "${bv}")"
        # The current variant.toml schema (`config/variant.rs::VariantImageBlock`)
        # collapses the old separate `os-image-publish-size-gib` and
        # `data-image-publish-size-gib` into a single hint
        # `publish-image-size-hint-gib`. Prefer the OS value if both are set.
        os_pub_size_gib="$(jq --raw-output '.["image-layout"]["os-image-publish-size-gib"] // empty' <<< "${bv}")"
        data_pub_size_gib="$(jq --raw-output '.["image-layout"]["data-image-publish-size-gib"] // empty' <<< "${bv}")"
        publish_size_hint_gib="${os_pub_size_gib:-${data_pub_size_gib}}"

        supported_arches_json="$(jq -c '.["supported-arches"] // []' <<< "${bv}")"
        features_json="$(jq -c '.["image-features"] // {}' <<< "${bv}")"
        kernel_params_json="$(jq -c '.["kernel-parameters"] // []' <<< "${bv}")"
        included_packages_json="$(jq -c '.["included-packages"] // []' <<< "${bv}")"

        # [variant.local-packages] is derived from the upstream Cargo
        # `[build-dependencies]` table, filtered to names that actually
        # exist as `packages/<name>/` in this repo. Upstream lists every
        # in-repo settings crate here (settings-defaults, settings-plugins,
        # settings-migrations, ...); we only emit the ones we've ported,
        # so the downstream `Variant::to_edo` transitive closure resolves.
        build_deps_json="$(jq -c '.["build-dependencies"] // {} | keys' <<< "${cargo_json}")"

        {
            echo 'schema-version = "3"'
            echo
            echo '[variant]'
            echo "name = \"${variant}\""
            echo "version = \"${version}\""

            # supported-arches (only emit if the source declares it; absent =
            # any arch, per `config/variant.rs::Variant::supported_arches`).
            n_arch="$(jq 'length' <<< "${supported_arches_json}")"
            if [ "${n_arch}" -gt 0 ]; then
                printf 'supported-arches = ['
                for i in $(seq 0 $((n_arch - 1))); do
                    [ "${i}" -gt 0 ] && printf ', '
                    a="$(jq --raw-output ".[${i}]" <<< "${supported_arches_json}")"
                    printf '"%s"' "${a}"
                done
                echo ']'
            fi

            # kernel-parameters (always emitted; empty array is fine).
            printf 'kernel-parameters = ['
            n_kp="$(jq 'length' <<< "${kernel_params_json}")"
            if [ "${n_kp}" -gt 0 ]; then
                echo
                for i in $(seq 0 $((n_kp - 1))); do
                    p="$(jq --raw-output ".[${i}]" <<< "${kernel_params_json}")"
                    printf '    "%s",\n' "${p}"
                done
            fi
            echo ']'

            # NOTE: no `[[variant.kit]]` block. The current variant schema
            # (`config/variant.rs::Variant`) has no `kit` field and rejects
            # unknown keys (`deny_unknown_fields`); every variant implicitly
            # depends on every project-level `[[kit]]` declared in
            # `Twoliter.toml` (see `config/mod.rs::Config::variant_toml`).

            # image sub-table: only emit if at least one field is set,
            # otherwise the parser would see an empty section header.
            if [ -n "${image_format}${partition_plan}${os_size_gib}${data_size_gib}${publish_size_hint_gib}" ]; then
                echo
                echo '[variant.image]'
                [ -n "${image_format}" ]           && echo "format = \"${image_format}\""
                [ -n "${partition_plan}" ]         && echo "partition-plan = \"${partition_plan}\""
                [ -n "${os_size_gib}" ]            && echo "os-image-size-gib = ${os_size_gib}"
                [ -n "${data_size_gib}" ]          && echo "data-image-size-gib = ${data_size_gib}"
                [ -n "${publish_size_hint_gib}" ]  && echo "publish-image-size-hint-gib = ${publish_size_hint_gib}"
            fi

            # image-features sub-table (only emit if non-empty).
            n_feat="$(jq 'length' <<< "${features_json}")"
            if [ "${n_feat}" -gt 0 ]; then
                echo
                echo '[variant.image.features]'
                # Emit in stable key order.
                jq --raw-output 'to_entries | sort_by(.key) | .[] | "\(.key) = \(.value)"' \
                    <<< "${features_json}"
            fi

            # included-packages sub-table (only emit if non-empty).
            n_pkg="$(jq 'length' <<< "${included_packages_json}")"
            if [ "${n_pkg}" -gt 0 ]; then
                echo
                echo '[variant.included-packages]'
                echo 'packages = ['
                for i in $(seq 0 $((n_pkg - 1))); do
                    p="$(jq --raw-output ".[${i}]" <<< "${included_packages_json}")"
                    printf '    "%s",\n' "${p}"
                done
                echo ']'
            fi

            # local-packages: build-dependencies that we've ported into
            # this repo (i.e. have a `packages/<name>/` directory). The
            # downstream `Variant::to_edo` transitively closes over each
            # package's `depends`, so only the top-level entries are
            # emitted here.
            local_pkgs=()
            n_bd="$(jq 'length' <<< "${build_deps_json}")"
            for i in $(seq 0 $((n_bd - 1))); do
                dep="$(jq --raw-output ".[${i}]" <<< "${build_deps_json}")"
                if [ -d "packages/${dep}" ]; then
                    local_pkgs+=("${dep}")
                fi
            done
            if [ "${#local_pkgs[@]}" -gt 0 ]; then
                echo
                echo '# In-repo packages built from `packages/<name>/package.toml`. Names'
                echo "# are transitively closed over each package's \`[package].depends\` at"
                echo '# emit time, so listing the top-level packages is enough.'
                echo '[variant.local-packages]'
                echo 'packages = ['
                for p in "${local_pkgs[@]}"; do
                    printf '    "%s",\n' "${p}"
                done
                echo ']'
            fi
        } > "${element}/variant.toml"
    done

# Generate `kits/<kit>/kit.toml` describing the kit and its package list.
_kit-toml kit:
    #!/usr/bin/env bash
    set -euo pipefail
    element="kits/{{kit}}"
    source="repos/{{kit}}"
    mkdir -p "${element}"

    version="$(tq --raw .release-version < "${source}/Twoliter.toml")"

    # Discover top-level go modules under sources/ (these become kit-wide
    # `go-modules` for the kit's go-vendor transform).
    go_modules=()
    while IFS= read -r modfile; do
        [ -z "${modfile}" ] && continue
        # Strip "<source>/" prefix and trailing "/go.mod" to get "sources/<name>".
        rel="${modfile#${source}/}"
        rel="${rel%/go.mod}"
        go_modules+=("${rel}")
    done < <(find "${source}/sources" -mindepth 2 -maxdepth 2 -name go.mod 2>/dev/null | sort)

    # The list of packages — every directory under `<source>/packages/`.
    packages=()
    while IFS= read -r pkgdir; do
        [ -z "${pkgdir}" ] && continue
        packages+=("$(basename "${pkgdir}")")
    done < <(find "${source}/packages" -mindepth 1 -maxdepth 1 -type d | sort)

    {
        echo 'schema-version = "3"'
        echo
        echo '[kit]'
        echo "name           = \"{{kit}}\""
        echo "version        = \"${version}\""
        echo "path           = \"${source}\""
        if [ "${#go_modules[@]}" -gt 0 ]; then
            printf 'go-modules     = ['
            for i in "${!go_modules[@]}"; do
                [ "${i}" -gt 0 ] && printf ', '
                printf '"%s"' "${go_modules[$i]}"
            done
            printf ']\n'
        fi
        echo 'cargo-projects = ["sources/Cargo.toml"]'
        echo 'packages       = ['
        for pkg in "${packages[@]}"; do
            echo "    '${pkg}',"
        done
        echo ']'
    } > "${element}/kit.toml"

# Generate `packages/<pkg_name>/package.toml` for a single package in `kit`.
# `package` is the upstream path, e.g. `packages/bash`.
sync-package kit package:
    #!/usr/bin/env bash
    set -euo pipefail
    source="repos/{{kit}}"
    pkg_name="$(sed -e 's|packages/||g' <<< "{{package}}")"
    element="packages/${pkg_name}"
    cargo_toml="${source}/{{package}}/Cargo.toml"
    mkdir -p "${element}"

    echo "converting package ${pkg_name} ({{package}})"

    # Load the whole Cargo.toml as JSON once; all subsequent lookups go
    # through jq with `// <default>` so missing keys don't blow up.
    cargo_json="$(tq -o json . < "${cargo_toml}")"

    # Build dependencies → bare names (kernel-X_Y → kernel-X.Y mapping applied
    # below at emission time).
    depends="$(jq --raw-output '(.["build-dependencies"] // {}) | keys | .[]' <<< "${cargo_json}")"

    # source-groups list (may be empty / absent).
    source_groups="$(jq --raw-output '(.package.metadata["build-package"]["source-groups"] // []) | .[]' <<< "${cargo_json}")"

    # external-files (raw JSON array we walk below).
    external_files_json="$(jq -c '.package.metadata["build-package"]["external-files"] // []' <<< "${cargo_json}")"
    n_ext="$(jq 'length' <<< "${external_files_json}")"

    # Is this package itself a top-level go module under sources/?
    is_go_module=false
    if [ -f "${source}/sources/${pkg_name}/go.mod" ]; then
        is_go_module=true
    fi

    go_major=""
    spec_file="${source}/{{package}}/${pkg_name}.spec"
    if [ -f "${spec_file}" ]; then
        raw="$(grep -E '^\s*export\s+GO_MAJOR\s*=' "${spec_file}" \
            | grep -v '^\s*#' \
            | tail -n 1 \
            | sed -E 's/^\s*export\s+GO_MAJOR\s*=\s*"?([^"]*)"?\s*$/\1/' \
            || true)"
        if [[ "${raw}" =~ ^[0-9]+\.[0-9]+$ ]]; then
            go_major="${raw}"
        elif [ -n "${raw}" ]; then
            echo "warning: skipping non-literal GO_MAJOR=\"${raw}\" in ${spec_file}" >&2
        fi
    fi

    # ----- write [package] header -----
    {
        echo 'schema-version = "3"'
        echo
        echo '[package]'
        echo "name = \"${pkg_name}\""
        echo "path = \"${source}/{{package}}\""
        if [ -n "${go_major}" ]; then
            echo "go-major = \"${go_major}\""
        fi
        # Collect dependencies (with kernel-X_Y → kernel-X.Y mapping).
        deps=()
        for dep in ${depends}; do
            case "${dep}" in
                kernel-5_10) dep="kernel-5.10" ;;
                kernel-5_15) dep="kernel-5.15" ;;
                kernel-6_1)  dep="kernel-6.1"  ;;
                kernel-6_12) dep="kernel-6.12" ;;
                kernel-6_18) dep="kernel-6.18" ;;
            esac
            deps+=("${dep}")
        done
        # Emit single-line for 0/1 deps, multi-line otherwise.
        if [ "${#deps[@]}" -eq 0 ]; then
            echo 'depends = []'
        elif [ "${#deps[@]}" -eq 1 ]; then
            echo "depends = [\"${deps[0]}\"]"
        else
            printf 'depends = [\n'
            for d in "${deps[@]}"; do
                printf '    "%s",\n' "${d}"
            done
            printf ']\n'
        fi
    } > "${element}/package.toml"

    # ----- write [[package.source]] entries -----
    if [ "${n_ext}" -eq 0 ]; then
        echo '' >> "${element}/package.toml"
        echo '[[package.source]]' >> "${element}/package.toml"
        echo "addr = \"//kits/{{kit}}/kit-src\"" >> "${element}/package.toml"
        if [ "${is_go_module}" = "true" ]; then
            echo "vendored = [\"//kits/{{kit}}/kit-go\"]" >> "${element}/package.toml"
        else
            echo "vendored = [\"//kits/{{kit}}/kit-rust\"]" >> "${element}/package.toml"
        fi
    else
        # Group external-files by URL while preserving first-seen order —
        # multiple entries with the same URL (e.g. soci-snapshotter declaring
        # different `bundle-root-path`s) collapse into a single source with
        # merged go-modules.
        grouped_json="$(jq -c '
            reduce .[] as $f (
                {order: [], by: {}};
                if (.by[$f.url] // null) == null then
                    .order += [$f.url] | .by[$f.url] = [$f]
                else
                    .by[$f.url] += [$f]
                end
            )
            | [ .order[] as $u | { url: $u, files: .by[$u] } ]
            | map({
                url: .url,
                digest: .files[0].sha512,
                out: ((.files[0].path) // (.url | split("/") | last)),
                files: .files,
            })
        ' <<< "${external_files_json}")"
        n_groups="$(jq 'length' <<< "${grouped_json}")"

        for i in $(seq 0 $((n_groups - 1))); do
            grp="$(jq -c ".[${i}]" <<< "${grouped_json}")"
            url="$(jq --raw-output '.url' <<< "${grp}")"
            digest="$(jq --raw-output '.digest' <<< "${grp}")"
            ref="sha512:${digest}"
            out_name="$(jq --raw-output '.out' <<< "${grp}")"
            # Rewrite the source URL to the Bottlerocket lookaside cache, keyed
            # by the tarball filename (last URL path segment) and its sha512.
            # edo's sha512 support means we can point at the mirror directly
            # instead of hitting the upstream host at build time.
            #
            # Skip the rewrite when any file in the group is marked
            # `force-upstream = true`. In old-twoliter (buildsys/src/cache.rs)
            # that flag means the artifact was never uploaded to the lookaside
            # mirror (typically license-gated NVIDIA blobs, kernel SRPMs, etc.),
            # so `cache.bottlerocket.aws` returns S3 403 for those keys.
            force_upstream="$(jq --raw-output '[.files[]["force-upstream"] // false] | any' <<< "${grp}")"
            if [ "${force_upstream}" != "true" ]; then
                filename="$(basename "${url}")"
                url="https://cache.bottlerocket.aws/${filename}/${digest}/${filename}"
            fi

            # Collect go-modules across every external-files entry sharing
            # this URL. Each entry yields zero or one module string.
            go_modules=()
            n_files="$(jq '.files | length' <<< "${grp}")"
            for j in $(seq 0 $((n_files - 1))); do
                ef="$(jq -c ".files[${j}]" <<< "${grp}")"
                is_go_bundle="$(jq -r '(.["bundle-modules"] // []) | index("go") != null' <<< "${ef}")"
                [ "${is_go_bundle}" = "true" ] || continue
                bundle_root="$(jq --raw-output '.["bundle-root-path"] // empty' <<< "${ef}")"
                bundle_output="$(jq --raw-output '.["bundle-output-path"] // empty' <<< "${ef}")"
                if [ -n "${bundle_root}" ]; then
                    if [ -n "${bundle_output}" ]; then
                        # Strip the `bundled-` prefix; the loader re-adds it.
                        short_out="${bundle_output#bundled-}"
                        go_modules+=("${bundle_root}@${short_out}")
                    else
                        go_modules+=("${bundle_root}")
                    fi
                else
                    mod="${out_name%.tar.gz}"
                    mod="${mod%.tgz}"
                    mod="$(sed -E 's/-v([0-9])/-\1/' <<< "${mod}")"
                    go_modules+=("${mod}")
                fi
            done

            {
                echo ''
                echo '[[package.source]]'
                echo "url  = \"${url}\""
                echo "ref  = \"${ref}\""
                echo "out  = \"${out_name}\""
            } >> "${element}/package.toml"

            if [ "${#go_modules[@]}" -gt 0 ]; then
                printf 'go-modules = [' >> "${element}/package.toml"
                for k in "${!go_modules[@]}"; do
                    [ "${k}" -gt 0 ] && printf ', ' >> "${element}/package.toml"
                    printf '"%s"' "${go_modules[$k]}" >> "${element}/package.toml"
                done
                printf ']\n' >> "${element}/package.toml"
            fi
        done

        # If the package ALSO declares source-groups, append a kit-src entry
        # after the URL sources (rare; mixes upstream tarballs with kit-local
        # Rust/Go sources).
        if [ -n "${source_groups}" ]; then
            {
                echo ''
                echo '[[package.source]]'
                echo "addr = \"//kits/{{kit}}/kit-src\""
                if [ "${is_go_module}" = "true" ]; then
                    echo "vendored = [\"//kits/{{kit}}/kit-go\"]"
                else
                    echo "vendored = [\"//kits/{{kit}}/kit-rust\"]"
                fi
            } >> "${element}/package.toml"
        fi
    fi

# Walk every package in the kit and emit its `package.toml`.
_sync-packages kit:
    #!/usr/bin/env bash
    set -euo pipefail
    source="repos/{{kit}}"
    packages="$(tq .workspace.members < "${source}/Cargo.toml" \
        | jq --raw-output '.[] | select(. | match("packages/";"i"))')"
    for package in ${packages}; do
        just sync-package {{kit}} "${package}"
    done

sync-kit kit:
    @rm -rf kits/{{kit}}
    @echo "creating kit.toml"
    @just _kit-toml {{kit}}
    @echo "creating package.toml files"
    @just _sync-packages {{kit}}

sync-bob:
    @rm -rf variants/*
    @rm -rf kits/bottlerocket
    @echo "creating variant.tomls"
    @just _variant-toml
    @echo "creating local kit.toml for bottlerocket (kit-src + kit-rust vendoring)"
    @just _kit-toml bottlerocket
    @echo "creating package.toml files"
    @just _sync-packages bottlerocket
