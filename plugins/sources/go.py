import os
import subprocess
from buildstream import DownloadableFileSource, SourceError, utils


class GoSource(DownloadableFileSource):
    # pylint: disable=attribute-defined-outside-init
    BST_MIN_VERSION = "2.3"

    def configure(self, node):
        super().configure(node)

        self.filename = node.get_str("filename", os.path.basename(self.url))
        self.executable = node.get_bool("executable", default=False)
        self.folder = node.get_str("folder")

        if os.sep in self.filename:
            raise SourceError(
                "{}: filename parameter cannot contain directories".format(self), reason="filename-contains-directory"
            )
        node.validate_keys(DownloadableFileSource.COMMON_CONFIG_KEYS + ["filename", "executable", "folder"])

    def get_unique_key(self):
        return super().get_unique_key() + [self.filename, self.executable]

    def stage(self, directory):
        # Same as in local plugin, don't use hardlinks to stage sources, they
        # are not write protected in the sandbox.
        dest = os.path.join(directory, self.filename)
        bundled = os.path.join(directory, f"bundled-{self.filename}")
        with self.timed_activity("Staging remote file to {}".format(dest)):

            utils.safe_copy(self._get_mirror_file(), dest)

            # To prevent user's umask introducing variability here, explicitly set
            # file modes.
            if self.executable:
                os.chmod(dest, 0o755)
            else:
                os.chmod(dest, 0o644)

            go = utils.get_host_tool("go")
            tar = utils.get_host_tool("tar")
            temp_path = os.path.join(directory, "vendor_fetch")
            os.mkdir(temp_path)
            temp_dir = utils._tempdir(dir=temp_path)
            folder = f"{temp_path}/{self.folder}"
            subprocess.run([
                tar,
                "-xf",
                dest,
            ], cwd=temp_path)
            subprocess.run([
                go,
                "list",
                "-mod=readonly",
                "./..."
            ], cwd=folder)
            subprocess.run([
                go,
                "mod",
                "vendor"
            ], cwd=folder)
            subprocess.run([
                tar,
                "czf",
                bundled,
                f"{self.folder}/vendor"
            ], cwd=temp_path)

def setup():
    return GoSource