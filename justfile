github := "bottlerocket-os"

setup:
    cargo install tomlq

clone kit:
    git clone https://github.com/{{github}}/{{kit}}.git repos/{{kit}}

sync-code kit:
    #!/usr/bin/env bash
    element="kits/{{kit}}"
    mkdir -p "${element}"
    cat <<-EOF > "${element}/edo.toml"
    schema-version = "1"

    [source.code]
    kind = "local"
    path = "repos/{{kit}}"
    out  = "."

    [transform.cargo-vendor]
    kind = "cargo-vendor"
    source = ["code"]
    cargo-tomls = { "code" = ["sources/Cargo.toml"] }
    EOF

    if [ "{{kit}}" == "bottlerocket-core-kit" ]; then
        cat <<-EOF >> "${element}/edo.toml"
        [transform.go-vendor]
        kind = "go-vendor"
        source = ["code"]
        modules = [ "sources/host-ctr" ]
    EOF
    fi

sync-sdk kit:
    #!/usr/bin/env bash
    element="kits/{{kit}}"
    source="repos/{{kit}}"
    registry="$(cat "${source}/Twoliter.toml" | tq --raw .vendor.bottlerocket.registry)"
    repository="$(cat "${source}/Twoliter.toml" | tq --raw .sdk.name)"
    tag="$(cat "${source}/Twoliter.toml" | tq --raw .sdk.version)"
    echo "Registry: ${registry}, Repository = ${repository}, Tag = ${tag}"
    cat <<EOF >> "${element}/edo.toml"
    [vendor.{{kit}}]
    kind = "image"
    uri  = "${registry}"

    [requires.bottlerocket-sdk]
    kind = "image"
    at   = "=${tag}"

    [environment.sdk]
    kind = "container"
    user = "root"
    source = ["bottlerocket-sdk"]
    EOF

sync-package kit package:
    #!/usr/bin/env bash
    element="{{package}}"
    mkdir -p "${element}"
    source="repos/{{kit}}"
    version="$(cat ${source}/Twoliter.toml | tq --raw .release-version)"
    pkg_name="$(sed -e 's|packages/||g' <<< "{{package}}")"
    downloads="$(cat "${source}/{{package}}/Cargo.toml" | tq -o json .package.metadata.build-package.external-files | jq --raw-output '.[].url' || echo ' ')"
    depend="$(cat "${source}/{{package}}/Cargo.toml" | tq -o json .build-dependencies | jq --raw-output '. | keys | .[]' || echo ' ')"
    source_groups="$(cat "${source}/{{package}}/Cargo.toml" | tq -o json .package.metadata.build-package.source-groups 2>/dev/null | jq --raw-output 'if . == null then "" else .[] end' || echo '')"
    echo "converting package ${pkg_name} ({{package}})"
    cat <<-EOF > "${element}/edo.toml"
    schema-version = "1"
    [source.src]
    kind    = "local"
    path    = "repos/{{kit}}/packages/${pkg_name}"
    out     = "."
    [transform.rpm]
    kind    = "rpm"
    sdk     = "//kits/{{kit}}/sdk"
    package = "${pkg_name}"
    depends = [
    EOF
    for dep in $depend; do
        if [[ "${dep}" == "kernel-5_10" ]]; then
            dep="kernel-5.10"
        elif [[ "${dep}" == "kernel-5_15" ]]; then
            dep="kernel-5.15"
        elif [[ "${dep}" == "kernel-6_1" ]]; then
            dep="kernel-6.1"
        elif [[ "${dep}" == "kernel-6_12" ]]; then
            dep="kernel-6.12"
        elif [[ "${dep}" == "kernel-6_18" ]]; then
            dep="kernel-6.18"
        fi
        cat <<-EOF >> "${element}/edo.toml"
          "//packages/${dep}/rpm",
    EOF
    done
    cat <<-EOF >> "${element}/edo.toml"
    ]
    EOF

    if [ -n "${source_groups}" ]; then
        cat <<-EOF >> "${element}/edo.toml"
        vendored = [
            "//kits/{{kit}}/cargo-vendor",
    EOF

        cat <<-EOF >> "${element}/edo.toml"
        ]
    EOF
    fi

    cat <<-EOF >> "${element}/edo.toml"
    [transform.rpm.source]
    sources = [
        "//packages/${pkg_name}/src",
    EOF
    if [ -n "${source_groups}" ]; then
        cat <<-EOF >> "${element}/edo.toml"
        "//kits/{{kit}}/code",
    EOF
    fi
    cat <<-EOF >> "${element}/edo.toml"
    ]
    archives = [
    EOF
    for item in $downloads; do
        filename="$(basename "${item}")"
        cat <<-EOF >> "${element}/edo.toml"
          "//${element}/${filename}",
    EOF
    done
    cat <<-EOF >> "${element}/edo.toml"
    ]
    EOF
    spec_file="${source}/{{package}}/${pkg_name}.spec"
    for item in $downloads; do
        filename="$(basename "${item}")"
        ref=$(curl -sL "${item}" | b3sum | awk '{ print $1 }' )
        cat <<-EOF >> "${element}/edo.toml"
      [source."${filename}"]
      kind = "remote"
      url  = "${item}"
      ref  = "${ref}"
      out  = "${filename}"
    EOF
    done
    # spec_file="${source}/{{package}}/${pkg_name}.spec"
    # go_found="$(cat "${spec_file}" | grep '1.22' || echo "")"
    # if [ -n "${go_found}" ]; then
    #     cat <<EOF >> "${element}/${pkg_name}.edo.bml"
    #     go-version = "1.22.1"
    # EOF
    # fi
    # cat <<EOF >> "${element}/${pkg_name}.edo.bml"
    # }
    # EOF

_sync-packages kit:
    #!/usr/bin/env bash
    element="kits/{{kit}}"
    source="repos/{{kit}}"
    cat <<-EOF >> "${element}/edo.toml"
    [transform.pkgs]
    kind = "compose"
    depends = [
    EOF
    packages="$(cat "${source}/Cargo.toml" | tq .workspace.members | jq --raw-output '.[] | select(. | match("packages/";"i"))')"
    IFS='\n' read -a package_list <<< "${packages}"
    for package in ${packages[@]}; do
        just sync-package {{kit}} "${package}"
        pkg_name="$(sed -e 's|packages/||g' <<< "${package}")"
        echo "'//packages/${pkg_name}/rpm'," >> "${element}/edo.toml"
    done
    cat <<-EOF >> "${element}/edo.toml"
        ]
    EOF

sync kit:
    @rm -rf kits/{{kit}}
    @echo "create code element"
    @just sync-code {{kit}}
    @echo "create sdk element"
    @just sync-sdk  {{kit}}
    @echo "creating pkgs element"
    @just _sync-packages {{kit}}

build kit arch:
    @echo "building all packages in {{kit}} for architecture {{arch}}"
    bst -o arch {{arch}} build kits/{{kit}}/pkgs.bst

build-package kit arch package:
    @echo "building package {{package}} in {{kit}} for architecture {{arch}}"
    bst -o arch {{arch}} build kits/{{kit}}/{{package}}.bst
view-package kit arch package:
    bst -o arch {{arch}} artifact list-contents kits/{{kit}}/{{package}}.bst
checkout-package kit arch package directory:
    bst -o arch {{arch}} artifact checkout kits/{{kit}}/{{package}}.bst --directory {{directory}}
