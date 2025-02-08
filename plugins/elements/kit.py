import os
import json
import base64

from buildstream import Element, utils

class KitElement(Element):
    BST_MIN_VERSION = "2.3"

    def configure(self, node):
        node.validate_keys(["name", "release-version", "sdk"])
        self.kit = node.get_str("name")
        self.arch = self.get_variable("arch")
        self.sdk = node.get_str("sdk")
        self.release_version = node.get_str("release-version")

    def preflight(self):
        pass

    def configure_dependencies(self, dependencies):
        self.__layout = {}
        for dep in dependencies:
            location = "/"
            if dep.config:
                dep.config.validate_keys(["location"])
                location = dep.config.get_str("location")
            try:
                element_list = self.__layout[location]
            except KeyError:
                element_list = []
                self.__layout[location] = element_list
            element_list.append((dep.element, dep.path))

    def get_unique_key(self):
        dictionary = {}
        dictionary['name'] = self.name
        dictionary['arch'] = self.arch
        dictionary['release-version'] = self.release_version
        if any(location for location in self.__layout if location != "/"):
            sorted_locations = sorted(self.__layout)
            layout_key = {
                location: [dependency_path for _, dependency_path in self.__layout[location]]
                for location in sorted_locations
            }
            dictionary["layout"] = layout_key
        return dictionary
    
    def configure_sandbox(self, sandbox):
        build_root = self.get_variable("build-root")
        install_root = self.get_variable("install-root")

        # Tell the sandbox to mount the build root and install root
        sandbox.mark_directory(build_root)
        sandbox.mark_directory(install_root)
        sandbox.set_work_directory(build_root)
        env_vars = self.get_environment()
        sandbox.set_environment(env_vars)

    def stage(self, sandbox):
        sorted_locations = sorted(self.__layout)
        for location in sorted_locations:
            with self.timed_activity("Staging dependencies at: {}".format(location), silent_nested=True):
                element_list = [element for element, _ in self.__layout[location]]
                self.stage_dependency_artifacts(sandbox, element_list, path=location)
        root_list = self.__layout.get("/", None)
        if root_list:
            element_list = [element for element, _ in root_list]
            with self.timed_activity("Integrating sandbox", silent_nested=True), sandbox.batch():
                for dep in self.dependencies(element_list):
                    dep.integrate(sandbox)

    def assemble(self, sandbox):
        with sandbox.batch(root_read_only=False, label="Constructing kit"):
            buildroot = self.get_variable("build-root")
            installroot = self.get_variable("install-root")
            
            docker_arch = self.get_variable("go-arch")
            vdir = sandbox.get_virtual_directory()
            rpm_vdir = vdir.open_directory(f"rpms/{self.arch}/Packages")

            build_vdir = vdir.open_directory(buildroot.lstrip(os.sep), create=True)
            kit_vdir = build_vdir.open_directory("kit", create=True)
            kit_vdir.open_directory("Packages", create=True)
            oci_vdir = build_vdir.open_directory("oci", create=True)
            build_vdir._create_empty_file("content-layer.tar")

            for package in rpm_vdir.list_relative_paths():
                if package.endswith(".rpm"):
                    continue
                self.__run_command(sandbox, f"mkdir -p {buildroot}/kit/Packages/{package}")
                self.__run_command(sandbox, f"find /rpms/{self.arch}/Packages/{package} -mindepth 1 -maxdepth 1 -name \"*.{self.arch}.rpm\" -size +0c -exec install -p -m 0644 -t {buildroot}/kits/Packages/{package} {{}} \\+")
            self.__run_command(sandbox, f"createrepo_c {buildroot}/kit")
            self.__run_command(sandbox, f"dnf --disablerepo '*' --repofrompath \"kit,file:///{buildroot}/kit\" repoquery")
            # digest from file: sha256sum | aws '{ print $1 };'
            blob_vdir = oci_vdir.open_directory("blobs/sha256", create=True)
            layer_digests = {}
            for package in kit_vdir.open_directory("Packages").list_relative_paths():
                if package == "repodata":
                    continue
                layer_archive = f"{buildroot}/content-layer.tar"
                self.__run_command(sandbox, f"tar -cvf {layer_archive} --sort=name -C {buildroot}/kit/Packages {package}")
                digest = build_vdir.file_digest("content-layer.tar")
                content = build_vdir.stat("content-layer.tar")
                self.__run_command(sandbox, f"mv {buildroot}/content-layer {buildroot}/kit/blobs/sha256/{digest}")
                layer_digests[digest] = content.size

            metadata = json.dumps({
                "name": self.kit,
                "version": self.release_version,
                "sdk": self.sdk,
                "kit": []
            })

            image_config = json.dumps({
                "architecture": docker_arch,
                "config": {
                    "Env": [],
                    "WorkingDir": "/",
                    "OnBuild": None,
                    "Labels": {
                        "dev.bottlerocket.kit.v2": self.__digest(metadata)
                    }
                },
                "created": "",
                "history": [],
                "os": "linux",
                "rootfs": {
                    "type": "layers",
                    "diff_ids": list(map(lambda x: f"sha256:{x}", layer_digests.keys()))
                }
            })
            config_digest = self.__digest(image_config)
            with blob_vdir.open_file(config_digest, mode="w") as file:
                file.write(image_config)
            
            manifest = json.dumps({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {
                    "mediaType": "application/vnd.oci.image.config.v1+json",
                    "digest": f"sha256:{config_digest}",
                    "size": len(image_config)
                },
                "layers": list(map(lambda x: {
                    "mediaType": "application/vnd.oci.image.layer.v1.tar",
                    "digest": x,
                    "size": layer_digests[x]
                }, layer_digests))
            })
            manifest_digest = self.__digest(manifest)
            with blob_vdir.open_file(manifest_digest, mode="w") as file:
                file.write(manifest)
            index = json.dumps({
                "schemaVersion": 2,
                "manifests": [
                    {
                        "mediaType": "applicaiton/vnd.oci.image.manifest.v1+json",
                        "digest": f"sha256:{manifest_digest}",
                        "size": len(manifest),
                        "annotations": {
                            "org.opencontainers.image.created": ""
                        },
                        "platform": {
                            "architecture": docker_arch,
                            "os": "linux"
                        }
                    }
                ]
            })
            with oci_vdir.open_file("index.json", mode="w") as file:
                file.write(index)
            with oci_vdir.open_file("oci-layout", mode="w") as file:
                file.write('{"imageLayoutVersion": "1.0.0"}')
            self.__run_command(sandbox, f"tar -cf {installroot}/{self.kit}-v{self.release_version}-{self.arch}.tar -C {buildroot}/oci .")
            return installroot
        
    def __digest(self, data):
        return base64.b64encode(data.encode('utf-8')).decode('utf-8')

    def __run_command(self, sandbox, cmd):
        sandbox.run(["sh", "-c", "-e", cmd + "\n"], root_read_only=False, label=cmd)

def setup():
    return KitElement