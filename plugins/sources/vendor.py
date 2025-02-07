

import os
import shutil
import subprocess
from buildstream import Source, SourceError, Directory, utils

class VendorSource(Source):
    # pylint: disable=attribute-defined-outside-init

    BST_MIN_VERSION = "2.0"
    BST_STAGE_VIRTUAL_DIRECTORY = True

    __digest = None
    host_cargo = None
    host_go = None

    def configure(self, node):
        node.validate_keys(["gopkgs", "path", *Source.COMMON_CONFIG_KEYS])
        self.path = self.node_get_project_path(node.get_scalar("path"))
        self.fullpath = os.path.join(self.get_project_directory(), self.path)
        self.gopkgs = node.get_str_list("gopkgs", [])

    def preflight(self):
        self.host_cargo = utils.get_host_tool("cargo")
        self.host_go = utils.get_host_tool("go")

    def is_resolved(self):
        return True

    def is_cached(self):
        return True

    def get_unique_key(self):
        #
        # As a core plugin, we use some private API to optimize file hashing.
        #
        # * Use Source._cache_directory() to prepare a Directory
        # * Do the regular staging activity into the Directory
        # * Use the hash of the cached digest as the unique key
        #
        if not self.__digest:
            with self._cache_directory() as directory:
                self.__do_stage(directory)
                self.__digest = directory._get_digest()

        return self.__digest.hash

    # We dont have a ref, we're a local file...
    def load_ref(self, node):
        pass

    def get_ref(self):
        return None  # pragma: nocover

    def set_ref(self, ref, node):
        pass  # pragma: nocover

    def fetch(self):  # pylint: disable=arguments-differ
        # Nothing to do here for a local source
        pass  # pragma: nocover

    def stage_directory(self, directory):
        #
        # We've already prepared the CAS while resolving the cache key which
        # will happen before staging.
        #
        # Now just retrieve the previously cached content to stage.
        #
        assert isinstance(directory, Directory)
        assert self.__digest is not None
        with self._cache_directory(digest=self.__digest) as cached_directory:
            directory.import_files(cached_directory, collect_result=False)

    def init_workspace_directory(self, directory):
        #
        # FIXME: We should be able to stage the workspace from the content
        #        cached in CAS instead of reimporting from the filesystem
        #        to the new workspace directory with this special case, but
        #        for some reason the writable bits are getting lost on regular
        #        files through the transition.
        #
        self.__do_stage(directory)

    # As a core element, we speed up some scenarios when this is used for
    # a junction, by providing the local path to this content directly.
    #
    def _get_local_path(self):
        return self.fullpath

    # Staging is implemented internally, we preemptively put it in the CAS
    # as a side effect of resolving the cache key, at stage time we just
    # do an internal CAS stage.
    #
    def __do_stage(self, directory):
        path = self.fullpath
        print(f"Path = {path}")
        vendored = os.path.join(path, "vendored")
        with self.timed_activity("Vendoring cargo and go dependencies"):
            source_dir = os.path.join(path, "sources")
            config = subprocess.check_output([
                self.host_cargo,
                "vendor"
            ], cwd=source_dir)
            cargo_dir = os.path.join(source_dir, ".cargo")
            if not os.path.exists(cargo_dir):
                os.mkdir(cargo_dir)
            config_file = os.path.join(cargo_dir, "config.toml")
            if os.path.exists(config_file):
                os.remove(config_file)
            with open(config_file, "wb") as file:
                file.write(config)
            for pkg in self.gopkgs:
                pkg_dir = os.path.join(source_dir, pkg)
                subprocess.run([
                    self.host_go,
                    "mod",
                    "vendor"
                ], cwd=pkg_dir)
        with self.timed_activity("Staging local files into CAS"):
            if os.path.isdir(self.fullpath) and not os.path.islink(self.fullpath):
                result = directory.import_files(self.fullpath)
            else:
                result = directory.import_single_file(self.fullpath)

            if result.overwritten or result.ignored:
                raise SourceError(
                    "Failed to stage source: files clash with existing directory", reason="ensure-stage-dir-fail"
                )


# Plugin entry point
def setup():
    return VendorSource