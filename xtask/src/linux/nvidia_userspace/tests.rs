use super::*;
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

const V: &str = "580.178.04";

/// An `ar` archive of `members`, laid out as `dpkg-deb` writes one.
fn ar(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, data) in members {
        let header = format!(
            "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            0,
            0,
            0,
            100644,
            data.len()
        );
        assert_eq!(header.len(), 60);
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

/// A fresh directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "eclipse-nvidia-userspace-{}-{}-{name}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `.deb` at `at` whose data holds `files` (`(path, contents)`), packed by
/// the same `tar` that unpacks it.
fn deb(at: &Path, files: &[(&str, &[u8])]) {
    let tree = at.with_extension("tree");
    for (path, contents) in files {
        let path = tree.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    let tarball = at.with_extension("tar");
    let ok = Command::new("tar")
        .arg("-cf")
        .arg(&tarball)
        .arg("-C")
        .arg(&tree)
        .arg(".")
        .status()
        .unwrap()
        .success();
    assert!(ok, "tar is needed to build the test's packages");
    let data = fs::read(&tarball).unwrap();
    fs::write(
        at,
        ar(&[
            ("debian-binary", b"2.0\n"),
            ("control.tar", b"x"),
            ("data.tar", &data),
        ]),
    )
    .unwrap();
    fs::remove_file(tarball).unwrap();
    fs::remove_dir_all(tree).unwrap();
}

#[test]
fn it_is_on_unless_turned_off() {
    for on in [None, Some("1"), Some("on"), Some("yes")] {
        assert!(wanted_by(on), "{on:?}");
    }
    for off in ["0", "off", "no", "false", "", " OFF "] {
        assert!(!wanted_by(Some(off)), "{off:?}");
    }
}

#[test]
fn a_deb_is_read_member_by_member() {
    let bytes = ar(&[
        ("debian-binary", b"2.0\n"),
        ("control.tar.zst", b"odd"),
        ("data.tar.zst", b"12345"),
    ]);
    assert_eq!(
        ar_members(&bytes),
        Some(vec![
            ("debian-binary", &b"2.0\n"[..]),
            ("control.tar.zst", &b"odd"[..]),
            ("data.tar.zst", &b"12345"[..]),
        ])
    );
    // GNU ar ends a name with '/'.
    let bytes = ar(&[("data.tar/", b"ab")]);
    assert_eq!(ar_members(&bytes), Some(vec![("data.tar", &b"ab"[..])]));
}

#[test]
fn an_archive_cut_short_or_malformed_is_refused() {
    let good = ar(&[("debian-binary", b"2.0\n"), ("data.tar", b"12345")]);
    assert!(ar_members(&good).is_some());
    for cut in [5, 8 + 30, good.len() - 2] {
        assert_eq!(ar_members(&good[..cut]), None, "cut at {cut}");
    }
    let mut magic = good.clone();
    magic[1] = b'?';
    assert_eq!(ar_members(&magic), None);
    let mut fmag = good.clone();
    fmag[8 + 58] = b'!';
    assert_eq!(ar_members(&fmag), None);
    let mut size = good;
    size[8 + 48] = b'x';
    assert_eq!(ar_members(&size), None);
    // An unreadable size is refused even where reading it as nothing would
    // leave no bytes over to trip on.
    let mut empty = ar(&[("data.tar", b"")]);
    empty[8 + 48] = b'x';
    assert_eq!(ar_members(&empty), None);
}

#[test]
fn the_listing_gives_this_versions_amd64_debs_only() {
    let html = r#"
        <a href="../">Parent</a>
        <a href="libnvidia-compute-535_580.178.04-0ubuntu1_amd64.deb">x</a>
        <a href="libnvidia-compute-580_580.178.04-0ubuntu1_amd64.deb">x</a>
        <a href="libnvidia-compute-580_580.178.04-0ubuntu1_amd64v3.deb">x</a>
        <a href="libnvidia-compute-580_580.178.04-0ubuntu1_i386.deb">x</a>
        <a href="libnvidia-compute-580_580.178.04-0ubuntu0.24.04.1_amd64.deb">x</a>
        <a href="libnvidia-compute-580_580.95.05-0ubuntu1_amd64.deb">x</a>
        <a href="libnvidia-compute-580_580.178.041-0ubuntu1_amd64.deb">x</a>
        <a href="sub/libnvidia-compute-580_580.178.04-0ubuntu2_amd64.deb">x</a>
        <a href="libnvidia-compute-580_580.178.04-0ubuntu1/../../../etc_amd64.deb">x</a>
        <a href="libnvidia-compute-580_580.178.04-0ubuntu1_amd64.deb">again</a>
    "#;
    assert_eq!(
        listed_debs(html, "libnvidia-compute-580_580.178.04-"),
        [
            "libnvidia-compute-580_580.178.04-0ubuntu0.24.04.1_amd64.deb",
            "libnvidia-compute-580_580.178.04-0ubuntu1_amd64.deb",
        ]
    );
    assert!(listed_debs(html, "nvidia-utils-580_580.178.04-").is_empty());
}

/// Every glibc binary names `/lib64/ld-linux-x86-64.so.2` as its loader, and
/// the loader looks for its libraries in the multiarch directory first.
#[test]
fn the_plan_puts_glibc_where_its_binaries_look() {
    let plan = plan(V);
    let runtime: Vec<&Take> = plan.runtime.iter().flat_map(|(_, t)| t).collect();
    assert!(runtime.contains(&&Take {
        from: "usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2".into(),
        to: "lib64/ld-linux-x86-64.so.2".into(),
    }));
    for lib in [
        "libc.so.6",
        "libm.so.6",
        "libdl.so.2",
        "libpthread.so.0",
        "librt.so.1",
        "libgcc_s.so.1",
    ] {
        let path = format!("usr/lib/x86_64-linux-gnu/{lib}");
        assert!(runtime.contains(&&same(&path)), "{lib}");
    }
    for (deb, _) in &plan.runtime {
        assert!(matches!(deb, Deb::Pinned { .. }), "{deb:?} is not pinned");
    }
}

#[test]
fn the_plan_takes_this_versions_libraries_and_links_their_sonames() {
    let plan = plan(V);
    assert_eq!(
        plan.nvidia
            .iter()
            .map(|(deb, _)| deb.clone())
            .collect::<Vec<_>>(),
        [
            Deb::Release {
                pool: "restricted/n/nvidia-graphics-drivers-580".into(),
                prefix: "libnvidia-compute-580_580.178.04-".into(),
            },
            Deb::Release {
                pool: "restricted/n/nvidia-graphics-drivers-580".into(),
                prefix: "nvidia-utils-580_580.178.04-".into(),
            },
        ]
    );
    let takes: Vec<&Take> = plan.nvidia.iter().flat_map(|(_, t)| t).collect();
    for path in [
        "usr/lib/x86_64-linux-gnu/libnvidia-ml.so.580.178.04",
        "usr/lib/x86_64-linux-gnu/libcuda.so.580.178.04",
        "usr/lib/x86_64-linux-gnu/libnvidia-ptxjitcompiler.so.580.178.04",
        "usr/bin/nvidia-smi",
    ] {
        assert!(takes.contains(&&same(path)), "{path}");
    }
    for (link, target) in [
        ("libnvidia-ml.so.1", "libnvidia-ml.so.580.178.04"),
        ("libcuda.so.1", "libcuda.so.580.178.04"),
        ("libcuda.so", "libcuda.so.1"),
        (
            "libnvidia-ptxjitcompiler.so.1",
            "libnvidia-ptxjitcompiler.so.580.178.04",
        ),
    ] {
        let link = format!("usr/lib/x86_64-linux-gnu/{link}");
        assert!(
            plan.links.contains(&(link.clone(), target.into())),
            "{link}"
        );
    }
    assert_eq!(plan.programs, ["usr/bin/nvidia-smi"]);
}

/// A small plan of the same shape: one pinned runtime package, and two
/// release packages, one with two libraries and one with a program, and a
/// link.
struct Fixture {
    root: PathBuf,
    rootfs: PathBuf,
    cache: PathBuf,
    plan: Plan,
    pinned: PathBuf,
    release: PathBuf,
    utils: PathBuf,
}

const RELEASE: &str = "nvtest-1_580.178.04-0ubuntu1_amd64.deb";
const UTILS: &str = "nvtest-utils-1_580.178.04-0ubuntu1_amd64.deb";

fn fixture(name: &str) -> Fixture {
    let root = scratch(name);
    let source = root.join("source");
    fs::create_dir_all(&source).unwrap();
    let pinned = source.join("libc6_test_amd64.deb");
    deb(
        &pinned,
        &[
            ("usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2", b"loader"),
            ("usr/lib/x86_64-linux-gnu/libc.so.6", b"libc"),
            ("usr/share/doc/libc6/changelog", b"not taken"),
        ],
    );
    let release = source.join(RELEASE);
    deb(
        &release,
        &[
            ("usr/lib/x86_64-linux-gnu/libnvtest.so.580.178.04", b"nvml"),
            ("usr/lib/x86_64-linux-gnu/libnvjit.so.580.178.04", b"jit"),
        ],
    );
    let utils = source.join(UTILS);
    deb(&utils, &[("usr/bin/nvtest", b"NVTEST 580.178.04 build")]);
    let sha256 = sha256_of(&pinned).expect("sha256sum is needed");
    let plan = Plan {
        runtime: vec![(
            Deb::Pinned {
                pool: "main/t/test",
                file: "libc6_test_amd64.deb",
                sha256: Box::leak(sha256.into_boxed_str()),
            },
            vec![
                Take {
                    from: "usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2".into(),
                    to: LOADER.into(),
                },
                same("usr/lib/x86_64-linux-gnu/libc.so.6"),
            ],
        )],
        nvidia: vec![
            (
                Deb::Release {
                    pool: "restricted/n/test".into(),
                    prefix: "nvtest-1_580.178.04-".into(),
                },
                vec![
                    same("usr/lib/x86_64-linux-gnu/libnvtest.so.580.178.04"),
                    same("usr/lib/x86_64-linux-gnu/libnvjit.so.580.178.04"),
                ],
            ),
            (
                Deb::Release {
                    pool: "restricted/n/test".into(),
                    prefix: "nvtest-utils-1_580.178.04-".into(),
                },
                vec![same("usr/bin/nvtest")],
            ),
        ],
        links: vec![(
            "usr/lib/x86_64-linux-gnu/libnvtest.so.1".into(),
            "libnvtest.so.580.178.04".into(),
        )],
        programs: vec!["usr/bin/nvtest".into()],
    };
    Fixture {
        rootfs: root.join("rootfs"),
        cache: root.join("cache"),
        root,
        plan,
        pinned,
        release,
        utils,
    }
}

impl Fixture {
    /// Every package already in the cache, as a machine given them by hand.
    fn cached(self) -> Self {
        fs::create_dir_all(&self.cache).unwrap();
        fs::copy(&self.pinned, self.cache.join("libc6_test_amd64.deb")).unwrap();
        fs::copy(&self.release, self.cache.join(RELEASE)).unwrap();
        fs::copy(&self.utils, self.cache.join(UTILS)).unwrap();
        self
    }

    /// A fetch that copies the package in, as a download would.
    fn fetch(&self) -> impl Fn(&Deb, &Path) -> Result<(), String> + '_ {
        move |deb, cache| {
            let (from, file) = match deb {
                Deb::Pinned { file, .. } => (&self.pinned, file.to_string()),
                Deb::Release { prefix, .. } if UTILS.starts_with(prefix.as_str()) => {
                    (&self.utils, UTILS.to_string())
                }
                Deb::Release { .. } => (&self.release, RELEASE.to_string()),
            };
            fs::copy(from, cache.join(file)).unwrap();
            Ok(())
        }
    }

    fn install(&self, fetch: &dyn Fn(&Deb, &Path) -> Result<(), String>) -> Result<(), String> {
        install_with(&self.rootfs, &self.cache, V, &self.plan, fetch)
    }

    fn read(&self, path: &str) -> Option<String> {
        fs::read_to_string(self.rootfs.join(path)).ok()
    }

    fn installed(&self) -> bool {
        self.rootfs.exists() && fs::read_dir(&self.rootfs).unwrap().next().is_some()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn offline(_: &Deb, _: &Path) -> Result<(), String> {
    panic!("nothing should be fetched");
}

#[test]
fn a_rootfs_gets_every_file_and_link() {
    let f = fixture("all").cached();
    assert_eq!(f.install(&offline), Ok(()));
    assert_eq!(
        f.read("lib64/ld-linux-x86-64.so.2").as_deref(),
        Some("loader")
    );
    assert_eq!(
        f.read("usr/lib/x86_64-linux-gnu/libc.so.6").as_deref(),
        Some("libc")
    );
    assert_eq!(
        f.read("usr/lib/x86_64-linux-gnu/libnvtest.so.1").as_deref(),
        Some("nvml"),
        "through the link"
    );
    assert_eq!(
        f.read("usr/lib/x86_64-linux-gnu/libnvjit.so.580.178.04")
            .as_deref(),
        Some("jit")
    );
    assert_eq!(
        fs::read_link(f.rootfs.join("usr/lib/x86_64-linux-gnu/libnvtest.so.1")).unwrap(),
        Path::new("libnvtest.so.580.178.04"),
        "a relative link, so it holds wherever the rootfs is mounted"
    );
    assert_eq!(
        f.read("usr/bin/nvtest").as_deref(),
        Some("NVTEST 580.178.04 build")
    );
    assert!(
        !f.rootfs.join("usr/share").exists(),
        "only what the plan takes"
    );
    assert!(
        !f.cache.join(format!("stage-{V}")).exists(),
        "the stage is cleared"
    );
}

#[test]
fn a_second_install_over_the_first_gives_the_same_rootfs() {
    let f = fixture("again").cached();
    assert_eq!(f.install(&offline), Ok(()));
    assert_eq!(f.install(&offline), Ok(()));
    assert_eq!(
        f.read("usr/lib/x86_64-linux-gnu/libnvtest.so.1").as_deref(),
        Some("nvml")
    );
}

#[test]
fn packages_not_in_the_cache_are_fetched_once_each() {
    let f = fixture("fetch");
    let calls = Cell::new(0);
    let fetch = f.fetch();
    let counted = |deb: &Deb, cache: &Path| {
        calls.set(calls.get() + 1);
        fetch(deb, cache)
    };
    assert_eq!(f.install(&counted), Ok(()));
    assert_eq!(calls.get(), 3);
    calls.set(0);
    assert_eq!(f.install(&counted), Ok(()));
    assert_eq!(calls.get(), 0, "the second build is offline");
}

#[test]
fn a_cached_pinned_package_with_another_hash_is_fetched_again() {
    let f = fixture("hash").cached();
    fs::write(f.cache.join("libc6_test_amd64.deb"), b"cut short").unwrap();
    assert_eq!(f.install(&f.fetch()), Ok(()));
    assert_eq!(
        f.read("usr/lib/x86_64-linux-gnu/libc.so.6").as_deref(),
        Some("libc")
    );
}

#[test]
fn a_failed_fetch_installs_nothing() {
    let f = fixture("nofetch");
    let fail = |_: &Deb, _: &Path| Err("no network".to_string());
    assert_eq!(f.install(&fail), Err("no network".to_string()));
    assert!(!f.installed());
    assert!(
        !f.cache.join(format!("stage-{V}")).exists(),
        "the stage is cleared"
    );
}

#[test]
fn a_fetch_that_leaves_nothing_is_an_error() {
    let f = fixture("empty");
    let nothing = |_: &Deb, _: &Path| Ok(());
    assert!(f.install(&nothing).is_err());
    assert!(!f.installed());
}

/// The glibc package is unpacked first, so this also shows that what was
/// already unpacked is not installed on its own.
#[test]
fn a_file_missing_from_its_package_installs_nothing() {
    let f = fixture("missing").cached();
    deb(
        &f.cache.join(RELEASE),
        &[("usr/lib/x86_64-linux-gnu/libnvtest.so.1", b"nvml")],
    );
    assert!(f.install(&offline).is_err());
    assert!(!f.installed());
}

#[test]
fn a_program_of_another_version_installs_nothing() {
    let f = fixture("version").cached();
    deb(
        &f.cache.join(UTILS),
        &[("usr/bin/nvtest", b"NVTEST 580.95.05 build")],
    );
    assert_eq!(
        f.install(&offline),
        Err("usr/bin/nvtest is not the 580.178.04 build".to_string())
    );
    assert!(!f.installed());
}

#[test]
fn a_rootfs_with_a_glibc_loader_keeps_its_glibc() {
    let f = fixture("theirs").cached();
    fs::create_dir_all(f.rootfs.join("lib64")).unwrap();
    fs::write(f.rootfs.join(LOADER), b"theirs").unwrap();
    assert_eq!(f.install(&offline), Ok(()));
    assert_eq!(f.read(LOADER).as_deref(), Some("theirs"));
    assert_eq!(f.read("usr/lib/x86_64-linux-gnu/libc.so.6"), None);
    assert_eq!(
        f.read("usr/lib/x86_64-linux-gnu/libnvtest.so.1").as_deref(),
        Some("nvml")
    );
}

#[test]
fn a_file_that_is_not_a_deb_is_refused() {
    let f = fixture("notdeb").cached();
    fs::write(f.cache.join(RELEASE), b"<html>404</html>").unwrap();
    assert!(f.install(&offline).unwrap_err().contains("is not a .deb"));
    assert!(!f.installed());
}

#[test]
fn removing_takes_nvidias_files_and_links_and_leaves_the_rest() {
    let f = fixture("remove").cached();
    assert_eq!(f.install(&offline), Ok(()));
    fs::write(
        f.rootfs.join("usr/lib/x86_64-linux-gnu/libother.so"),
        b"other",
    )
    .unwrap();
    assert_eq!(remove_with(&f.rootfs, &f.plan), 4);
    for gone in [
        "usr/lib/x86_64-linux-gnu/libnvtest.so.580.178.04",
        "usr/lib/x86_64-linux-gnu/libnvjit.so.580.178.04",
        "usr/lib/x86_64-linux-gnu/libnvtest.so.1",
        "usr/bin/nvtest",
    ] {
        assert!(
            fs::symlink_metadata(f.rootfs.join(gone)).is_err(),
            "{gone} is still there"
        );
    }
    assert_eq!(f.read(LOADER).as_deref(), Some("loader"));
    assert_eq!(
        f.read("usr/lib/x86_64-linux-gnu/libc.so.6").as_deref(),
        Some("libc")
    );
    assert_eq!(
        f.read("usr/lib/x86_64-linux-gnu/libother.so").as_deref(),
        Some("other")
    );
    assert_eq!(remove_with(&f.rootfs, &f.plan), 0, "nothing left to take");
}
