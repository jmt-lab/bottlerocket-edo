import os
import subprocess

from buildstream import Source, SourceError, utils

class OCISource(Source):
    BST_MIN_VERSION = "2.3"

    def configure(self, node):
        self.registry = node.get_str("registry")
        self.repository = node.get_str("repository")
        self.tag = node.get_str("tag")
        self.uri = f"{self.registry}/{self.repository}:{self.tag}"

        node.validate_keys(["registry", "repository", "tag", "ref"])
        self._mirror_dir = os.path.join(self.get_mirror_directory(), utils.url_directory_name(self.uri))

    def get_unique_key(self):
        return [self.registry, self.repository, self.tag]

    def get_ref(self):
        crane = utils.get_host_tool("crane")
        hash = subprocess.check_output([
            crane,
            "digest",
            self.uri
        ])
        digest = str(hash).split(":")[1]
        digest = digest[0:len(digest)-3]
        self.ref = digest
        return bytes(self.ref, encoding="utf-8")
    def set_ref(self, ref, node):
        pass  # pragma: nocover
        
    def is_cached(self):
        return os.path.isfile(self._get_mirror_file())

    def preflight(self):
        return

    def fetch(self):
        if os.path.isfile(self._get_mirror_file()):
            print("Fetch skipped")
            return

        self._ensure_mirror()

    def _ensure_mirror(self):
        with self.tempdir() as td:
            if self.is_cached():
                return
            if self.ref is None:
                self.get_ref()

            try:
                os.makedirs(self._mirror_dir, exist_ok=True)
            except FileExistsError as e:
                raise SourceError(
                    "{}: Mirror directory exists but is not a directory: {}".format(self, self._mirror_dir)
                ) from e
            crane = utils.get_host_tool("crane")
            target_file = os.path.join(self._mirror_dir, str(self.ref))
            subprocess.run([
                crane,
                "export",
                self.uri,
                target_file
            ])

    def _get_mirror_file(self):
        return os.path.join(self._mirror_dir, str(self.ref))

    def stage(self, directory):
        with self.timed_activity("Staging oci artifact"):
            tar = utils.get_host_tool("tar")
            chmod = utils.get_host_tool("chmod")
            chown = utils.get_host_tool("chown")
            archive = os.path.join(directory, "oci.tar")
            utils.safe_copy(self._get_mirror_file(), archive)
            subprocess.run([
                tar,
                "-xf",
                archive
            ], cwd=directory)
            subprocess.run([
                chmod,
                "-R",
                "a+rwx",
                directory
            ])
            bash = os.path.join(directory, "bin/bash")
            target = os.path.join(directory, "bin/sh")
            if os.path.isfile(bash):
                utils.safe_copy(bash, target)
            
            os.remove(archive)

def setup():
    return OCISource
            