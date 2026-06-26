github := "bottlerocket-os"

setup:
    cargo install tomlq

clone kit:
    git clone https://github.com/{{github}}/{{kit}}.git repos/{{kit}}

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

    # ----- write [package] header -----
    {
        echo 'schema-version = "3"'
        echo
        echo '[package]'
        echo "name = \"${pkg_name}\""
        echo "path = \"${source}/{{package}}\""
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
        # No external files → use the kit-wide source. Pick vendoring based on
        # whether this package is a go module under sources, or otherwise
        # default to the kit's cargo-vendor output.
        echo '' >> "${element}/package.toml"
        echo '[[package.source]]' >> "${element}/package.toml"
        echo "addr = \"//kits/{{kit}}/kit-src\"" >> "${element}/package.toml"
        if [ "${is_go_module}" = "true" ]; then
            echo "vendored = [\"//kits/{{kit}}/kit-go\"]" >> "${element}/package.toml"
        else
            # TODO: Make optional — some packages (e.g. selinux-policy) don't
            # actually need rust vendoring; remove this line manually if so.
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
                out: ((.files[0].path) // (.url | split("/") | last)),
                files: .files,
            })
        ' <<< "${external_files_json}")"
        n_groups="$(jq 'length' <<< "${grouped_json}")"

        for i in $(seq 0 $((n_groups - 1))); do
            grp="$(jq -c ".[${i}]" <<< "${grouped_json}")"
            url="$(jq --raw-output '.url' <<< "${grp}")"
            out_name="$(jq --raw-output '.out' <<< "${grp}")"

            # Compute the BLAKE3 hash of the upstream file (once per URL).
            ref="$(curl -sL "${url}" | sha256sum | awk '{ print $1 }')"

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
                    # No bundle-root-path: derive from out filename, stripping
                    # `.tar.gz`/`.tgz` and the `-v` before a digit (GitHub
                    # archive convention: `archive/v1.2.3` → `<name>-1.2.3/`).
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

sync kit:
    @rm -rf kits/{{kit}}
    @echo "creating kit.toml"
    @just _kit-toml {{kit}}
    @echo "creating package.toml files"
    @just _sync-packages {{kit}}
