//! `/sys/module/<module>/parameters/<param>`: the boot parameters, the way
//! Linux shows them.
//!
//! On Linux a `nvidia.foo=1` on the command line is a parameter of the module
//! `nvidia`, and `kernel/params.c` publishes every one of them as a file under
//! `/sys/module/nvidia/parameters/`. That file is how userspace asks what the
//! kernel actually decided -- `cat /sys/module/nvidia_drm/parameters/modeset`
//! is the canonical "is KMS on?" test -- instead of grepping `/proc/cmdline`,
//! which answers yes to `nvidia.nouveau_uapi=0` and knows nothing of a flag
//! the kernel turned down because the hardware could not take it.
//!
//! Eclipse has no modules, but the `nvidia.*` flags are named exactly like
//! module parameters, so they get the same files. The spelling of the values
//! is Linux's too: a `bool` reads `Y` or `N` (`param_get_bool`) and a string
//! that was never set reads `(null)` (`param_get_charp`). They are read-only,
//! as a `0444` parameter is: everything here is decided once at boot.
//!
//! Each value is what the kernel *applied*, not only what was written:
//! `nouveau_uapi` reads `N` on a machine with no card that can serve it even
//! with the flag on the line, which is what Linux shows when a module refused
//! a value.

use super::*;
use kernel_hal::cmdline::NvidiaParams;

/// The modules this tree has a directory for.
const MODULES: &[&str] = &["nvidia"];

/// The `nvidia` parameters, in the order `ls` shows them.
const NVIDIA_PARAMS: &[&str] = &[
    "compute",
    "cuda",
    "hwcursor",
    "monitor",
    "multigpu",
    "nouveau_uapi",
    "nvkms",
    "wlr_gles2",
    "wlr_pixman",
    "wlr_vulkan",
];

/// What the kernel applied for the `nvidia.*` parameters, as `read()` returns
/// them. Split from the live lookups so a test can hand in each answer: the
/// real cmdline in a host build is the empty string.
pub(super) struct NvidiaParamFiles {
    /// What the command line asked for.
    pub asked: NvidiaParams,
    /// The nouveau uAPI as the driver latched it: on only when the flag was
    /// given *and* a card could serve it (`zCore/src/main.rs`).
    pub nouveau_uapi: bool,
    /// The display-engine cursor as the DRM core will use it.
    pub hwcursor: bool,
}

fn yn(v: bool) -> String {
    String::from(if v { "Y\n" } else { "N\n" })
}

fn bdf(v: Option<(u8, u8, u8)>) -> String {
    match v {
        Some((b, d, f)) => format!("{:02x}.{:02x}.{:x}\n", b, d, f),
        None => String::from("(null)\n"),
    }
}

impl NvidiaParamFiles {
    pub(super) fn read(&self, name: &str) -> Option<String> {
        let a = &self.asked;
        Some(match name {
            "compute" => bdf(a.compute),
            // The umbrella itself; what it turns on is in `/dev/nvidia*`.
            "cuda" => yn(a.cuda),
            "hwcursor" => yn(self.hwcursor),
            "monitor" => bdf(a.monitor),
            // What was declared. Whether a second card exists is a question
            // for `/sys/class/drm`, not for a parameter.
            "multigpu" => yn(a.multigpu),
            "nouveau_uapi" => yn(self.nouveau_uapi),
            // What was asked. Whether nvkms took a card is in the boot log,
            // one `[nvkms]` block per GPU.
            "nvkms" => yn(a.nvkms),
            "wlr_gles2" => yn(a.wlr_gles2),
            "wlr_pixman" => yn(a.wlr_pixman),
            "wlr_vulkan" => yn(a.wlr_vulkan),
            _ => return None,
        })
    }
}

fn read_nvidia_param(name: &str) -> Option<String> {
    NvidiaParamFiles {
        asked: NvidiaParams::boot(),
        nouveau_uapi: zcore_drivers::display::nouveau_uapi_enabled(),
        hwcursor: crate::fs::devfs::drm::hw_cursor_wanted(),
    }
    .read(name)
}

/// Which directory of the `module` subtree a node is.
#[derive(Clone, Copy)]
pub(super) enum ModuleDir {
    /// `/sys/module`
    Root,
    /// `/sys/module/<MODULES[i]>`
    Module(usize),
    /// `/sys/module/<MODULES[i]>/parameters`
    Parameters(usize),
}

impl ModuleDir {
    fn ino(self) -> usize {
        match self {
            Self::Root => ino::MODULE,
            Self::Module(i) => ino::MODULE_DIR + 2 * i,
            Self::Parameters(i) => ino::MODULE_DIR + 2 * i + 1,
        }
    }

    fn parent(self) -> Arc<dyn INode> {
        match self {
            Self::Root => SYS_ROOT.clone(),
            Self::Module(_) => Arc::new(ModuleDirINode(Self::Root)),
            Self::Parameters(i) => Arc::new(ModuleDirINode(Self::Module(i))),
        }
    }

    fn entries(self) -> &'static [&'static str] {
        match self {
            Self::Root => MODULES,
            Self::Module(_) => &["parameters"],
            Self::Parameters(_) => NVIDIA_PARAMS,
        }
    }
}

pub(super) struct ModuleDirINode(pub(super) ModuleDir);

impl INode for ModuleDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(self.0.ino()))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match (self.0, name) {
            (_, ".") => Ok(Arc::new(ModuleDirINode(self.0))),
            (_, "..") => Ok(self.0.parent()),
            (ModuleDir::Root, m) => MODULES
                .iter()
                .position(|x| *x == m)
                .map(|i| Arc::new(ModuleDirINode(ModuleDir::Module(i))) as Arc<dyn INode>)
                .ok_or(FsError::EntryNotFound),
            (ModuleDir::Module(i), "parameters") => {
                Ok(Arc::new(ModuleDirINode(ModuleDir::Parameters(i))))
            }
            (ModuleDir::Module(_), _) => Err(FsError::EntryNotFound),
            (ModuleDir::Parameters(_), p) => read_nvidia_param(p)
                .map(|v| Arc::new(Pseudo::new(&v, FileType::File)) as Arc<dyn INode>)
                .ok_or(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, self.0.entries())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(cmdline: &str) -> NvidiaParamFiles {
        let asked = NvidiaParams::parse(cmdline);
        NvidiaParamFiles {
            asked,
            nouveau_uapi: false,
            hwcursor: asked.hwcursor,
        }
    }

    #[test]
    fn a_bool_reads_the_way_linux_prints_one() {
        let p = files("LOG=warn:nvidia.cuda:nvidia.multigpu=0");
        assert_eq!(p.read("cuda").as_deref(), Some("Y\n"));
        assert_eq!(p.read("multigpu").as_deref(), Some("N\n"));
    }

    #[test]
    fn a_flag_turned_off_reads_n_which_grep_cannot_tell() {
        // `grep -q nvidia.cuda /proc/cmdline` says yes to this line.
        let p = files("nvidia.cuda=0");
        assert_eq!(p.read("cuda").as_deref(), Some("N\n"));
    }

    #[test]
    fn nouveau_uapi_is_what_the_driver_latched_not_what_was_written() {
        // The flag is on the line, but no card could serve it.
        let p = files("nvidia.nouveau_uapi");
        assert_eq!(p.read("nouveau_uapi").as_deref(), Some("N\n"));
        let p = NvidiaParamFiles {
            nouveau_uapi: true,
            ..files("")
        };
        assert_eq!(p.read("nouveau_uapi").as_deref(), Some("Y\n"));
    }

    #[test]
    fn an_unset_monitor_reads_like_an_unset_charp() {
        assert_eq!(files("").read("monitor").as_deref(), Some("(null)\n"));
        let p = files("LOG=warn:nvidia.monitor=65.00.0");
        assert_eq!(p.read("monitor").as_deref(), Some("65.00.0\n"));
    }

    #[test]
    fn nvkms_has_a_file() {
        assert_eq!(files("nvidia.nvkms").read("nvkms").as_deref(), Some("Y\n"));
        assert_eq!(files("").read("nvkms").as_deref(), Some("N\n"));
    }

    #[test]
    fn the_wlroots_renderer_flags_are_there_too() {
        let p = files("nvidia.nouveau_uapi:nvidia.wlr_vulkan");
        assert_eq!(p.read("wlr_vulkan").as_deref(), Some("Y\n"));
        assert_eq!(p.read("wlr_gles2").as_deref(), Some("N\n"));
        assert_eq!(p.read("wlr_pixman").as_deref(), Some("N\n"));
    }

    #[test]
    fn every_listed_parameter_has_a_file_and_nothing_else_does() {
        let p = files("");
        for name in NVIDIA_PARAMS {
            assert!(p.read(name).is_some(), "{} listed but has no value", name);
        }
        assert!(p.read("modeset").is_none());
    }

    #[test]
    fn the_tree_resolves_from_the_root() {
        let f = SYS_ROOT
            .lookup_follow("module/nvidia/parameters/monitor", 4)
            .unwrap();
        assert_eq!(f.metadata().unwrap().type_, FileType::File);
        assert_eq!(f.metadata().unwrap().mode, 0o444);
    }
}
