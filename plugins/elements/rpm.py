import os
from buildstream import Element, utils

class RPMElement(Element):
    BST_MIN_VERSION = "2.3"

    def configure(self, node):
        node.validate_keys(["package", "go-version", "release-version"])
        self.package = node.get_str("package")
        self.arch = self.get_variable("arch")
        self.go_version = node.get_str("go-version")
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
        dictionary['package'] = self.package
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
        env_vars["GO_VERSION"] = self.go_version
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
        self.stage_sources(sandbox, f"{self.get_variable("build-root")}/rpmbuild/SOURCES")
    
    def assemble(self, sandbox):
        with sandbox.batch(root_read_only=False, label="Building package"):
            marker_filename = ".bst-prepared"
            vdir = sandbox.get_virtual_directory()
            buildroot = self.get_variable("build-root")
            buildroot_vdir = vdir.open_directory(buildroot.lstrip(os.sep))
            # Check if we have already configured the build
            if not buildroot_vdir.exists(marker_filename):
                self.__run_command(sandbox, f"mkdir -p {buildroot}/rpmbuild/{{BUILD,RPMS,SOURCES,SPECS,SRPMS,tmp}}")
                self.__run_command(sandbox, f"mkdir -p {buildroot}/rpmbuild/{self.arch}")
                if vdir.exists("rpms"):
                    self.__run_command(sandbox, f"cp -rf /rpms/{self.arch}/* {buildroot}/rpmbuild/RPMS/")
                self.__run_command(sandbox, f"cp /usr/lib/rpm/platform/{self.arch}-bottlerocket/macros /etc/rpm/macros")
                # Create the rpm repo
                self.__run_command(sandbox, f"createrepo_c -o {buildroot}/rpmbuild/RPMS -x '*-debug-info-*.rpm' -x '*-debugsource-*.rpm' --no-database {buildroot}/rpmbuild/RPMS")

                # Copy the spec file to the right place
                self.__run_command(sandbox, f"cat /src/packages/{self.package}/{self.package}.spec | sed '/BuildRequires: %{{_cross_os}}glibc-devel/d' > {buildroot}/rpmbuild/SPECS/{self.package}.spec")
                self.__run_command(sandbox, f"cp -rf /src/sources {buildroot}/rpmbuild/BUILD/sources")
                self.__run_command(sandbox, f"mv {buildroot}/rpmbuild/BUILD/sources/vendor {buildroot}/rpmbuild/vendor")
                self.__run_command(sandbox, f"cp -rf /src/sources/.cargo {buildroot}/rpmbuild/.cargo")
                # Run the dnf prep
                self.__run_command(sandbox, f"dnf --assumeyes --disablerepo '*' --repofrompath repo,{buildroot}/rpmbuild/RPMS --enablerepo 'repo' --nogpgcheck --forcearch \"{self.arch}\" builddep {buildroot}/rpmbuild/SPECS/{self.package}.spec")
                # Copy the sources 
                self.__run_command(sandbox, f"find /src/packages/{self.package} -maxdepth 1 -not -path '*/\\.*' -type f -exec cp {{}} {buildroot}/rpmbuild/SOURCES/ \\;")
                buildroot_vdir._create_empty_file(marker_filename)
            # This is the big build
            self.__run_command(sandbox, f"rpmbuild -bb --clean --undefine _auto_set_build_flags --define \"_target_cpu {self.arch}\" --define \"dist {self.release_version}.dirty.br1\" --define \"_topdir {buildroot}/rpmbuild\" {buildroot}/rpmbuild/SPECS/{self.package}.spec")
            install_root = self.get_variable("install-root")
            self.__run_command(sandbox, f"mkdir -p {install_root}/rpms/{self.arch}/Packages/{self.package}")
            self.__run_command(sandbox, f"cp -rf {buildroot}/rpmbuild/RPMS/{self.arch}/bottlerocket-*.rpm {install_root}/rpms/{self.arch}/Packages/{self.package}")
            return install_root

    def __run_command(self, sandbox, cmd):
        sandbox.run(["sh", "-c", "-e", cmd + "\n"], root_read_only=False, label=cmd)

def setup():
    return RPMElement