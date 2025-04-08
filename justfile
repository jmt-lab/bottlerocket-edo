github := "jmt-lab"

setup:
    cargo install tomlq

clone kit:
    git clone git@github.com:{{github}}/{{kit}}.git repos/{{kit}}

sync-code kit:
    #!/usr/bin/env bash
    element="kits/{{kit}}"
    mkdir -p "${element}"
    if [ "{{kit}}" == "bottlerocket-core-kit" ]; then
        gopkgs="[\"host-ctr\"]"
    else
        gopkgs="[]"
    fi
    cat <<-EOF > "${element}/code.edo.bml"
    transform :import "code" {
      source :vendor "git" {
        path = "repos/{{kit}}"
        rust = true
        inside = "sources"
        out = "."
        go = ${gopkgs}
      }
    }
    EOF

sync-sdk kit:
    #!/usr/bin/env bash
    element="kits/{{kit}}"
    source="repos/{{kit}}"
    registry="$(cat "${source}/Twoliter.toml" | tq --raw .vendor.bottlerocket.registry)"
    repository="$(cat "${source}/Twoliter.toml" | tq --raw .sdk.name)"
    tag="$(cat "${source}/Twoliter.toml" | tq --raw .sdk.version)"
    echo "Registry: ${registry}, Repository = ${repository}, Tag = ${tag}"
    cat <<EOF > "${element}/sdk.edo.bml"
    vendor :oci "{{kit}}" {
        url = "localhost:5000"
    }
    environment :container "sdk" {
        wants :oci "bottlerocket-sdk" {
            at = ${tag}
        }
    }
    EOF

sync-package kit package:
    #!/usr/bin/env bash
    element="kits/{{kit}}"
    source="repos/{{kit}}"
    version="$(cat ${source}/Twoliter.toml | tq --raw .release-version)"
    pkg_name="$(sed -e 's|packages/||g' <<< "{{package}}")"
    downloads="$(cat "${source}/{{package}}/Cargo.toml" | tq -o json .package.metadata.build-package.external-files | jq --raw-output '.[].url' || echo ' ')"
    depend="$(cat "${source}/{{package}}/Cargo.toml" | tq -o json .build-dependencies | jq --raw-output '. | keys | .[]' || echo ' ')"
    echo "converting package ${pkg_name} ({{package}})"
    cat <<-EOF > "${element}/${pkg_name}.edo.bml"
    transform "bplugin:rpm" "${pkg_name}" {
      environment = "//kits/{{kit}}/sdk"
      package     = "${pkg_name}"
      arch        = "x86_64"
      release     = "${version}"
      depends     = [
        "//kits/{{kit}}/code"
      ]
      requires    = [
    EOF
    for dep in $depend; do
        if [[ "${dep}" == "kernel-5_10" ]]; then
            dep="kernel-5.10"
        elif [[ "${dep}" == "kernel-5_15" ]]; then
            dep="kernel-5.15"
        elif [[ "${dep}" == "kernel-6_1" ]]; then
            dep="kernel-6.1"
        elif [[ "${dep}" == "glibc" ]]; then
            continue
        fi
        cat <<-EOF >> "${element}/${pkg_name}.edo.bml"
          "//kits/{{kit}}/${dep}",
    EOF
    done
    cat <<'EOF' >> "${element}/${pkg_name}.edo.bml"
      ]
    EOF
    for item in $downloads; do
        filename="$(basename "${item}")"
        ref=$(curl -sL "${item}" | b3sum | awk '{ print $1 }' )
        cat <<-EOF >> "${element}/${pkg_name}.edo.bml"
      source :remote "${filename}" {
        url = "${item}"
        ref = "${ref}"
        out = "${filename}"
      }
    EOF
    done
    spec_file="${source}/{{package}}/${pkg_name}.spec"
    go_found="$(cat "${spec_file}" | grep '1.22' || echo "")"
    if [ -n "${go_found}" ]; then
        cat <<EOF >> "${element}/${pkg_name}.edo.bml"
        go-version = "1.22.1"
    EOF
    fi
    cat <<EOF >> "${element}/${pkg_name}.edo.bml"
    }
    EOF

_sync-packages kit:
    #!/usr/bin/env bash
    element="kits/{{kit}}"
    source="repos/{{kit}}"
    cat <<-EOF > "${element}/pkgs.edo.bml"
    transform :compose "pkgs" {
        depends = [
    EOF
    packages="$(cat "${source}/Cargo.toml" | tq .workspace.members | jq --raw-output '.[] | select(. | match("packages/";"i"))')"
    IFS='\n' read -a package_list <<< "${packages}"
    for package in ${packages[@]}; do
        just sync-package {{kit}} "${package}"
        pkg_name="$(sed -e 's|packages/||g' <<< "${package}")"
        echo "'//kits/{{kit}}/${pkg_name}'," >> "${element}/pkgs.edo.bml"
    done
    cat <<-EOF >> "${element}/pkgs.edo.bml"
        ]
    }
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
