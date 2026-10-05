use super::join_path_env;
use crate::{commands::fetch_online, Arch, REPOS};
use os_xtask_utils::{dir, CommandExt, Ext, Git, Make};
use std::{fs, path::Path};

impl super::LinuxRootfs {
    pub fn put_ffmpeg(&self) {
        // 递归 rootfs
        let musl = self.put_musl_libs();
        // 拉 ffmpeg
        let ffmpeg = REPOS.join("ffmpeg");
        if !ffmpeg.is_dir() {
            fetch_online!(ffmpeg, |tmp| {
                Git::clone("https://github.com/FFmpeg/FFmpeg.git")
                    .dir(tmp)
                    .branch("release/5.0")
                    .single_branch()
                    .depth(1)
                    .done()
            });
        }
        // 拷贝到目标路径
        let build = self.0.target().join("ffmpeg");
        dircpy::copy_dir(ffmpeg, &build).unwrap();
        // 构建
        match self.0 {
            Arch::Riscv64 => {
                let path_with_musl_gcc = join_path_env(&[musl.join("bin")]);
                println!("Configuring ffmpeg, please wait...");
                Ext::new("./configure")
                    .current_dir(&build)
                    .arg("--enable-cross-compile")
                    .arg("--cross-prefix=riscv64-linux-musl-")
                    .arg("--arch=riscv64")
                    .arg("--target-os=linux")
                    .arg("--enable-static")
                    .arg("--enable-shared")
                    .arg("--disable-doc")
                    .arg(format!(
                        "--prefix={}",
                        build.canonicalize().unwrap().join("install").display(),
                    ))
                    .env("PATH", &path_with_musl_gcc)
                    .invoke();
                Make::install()
                    .current_dir(&build)
                    .j(num_cpus::get().min(8)) // 不能用太多线程，以免爆内存
                    .env("PATH", path_with_musl_gcc)
                    .invoke();
            }
            Arch::X86_64 | Arch::Aarch64 => todo!(),
        }
        // 拷贝
        self.put_libs(musl, build.join("install"));
    }

    pub fn put_opencv(&self) {
        // 递归 rootfs
        let musl = self.put_musl_libs();
        // 拉 opencv
        let opencv = REPOS.join("opencv");
        if !opencv.is_dir() {
            fetch_online!(opencv, |tmp| {
                Git::clone("https://github.com/opencv/opencv.git")
                    .dir(tmp)
                    .single_branch()
                    .depth(1)
                    .done()
            });
        }
        let source = opencv.canonicalize().unwrap();
        let target = self.0.target().join("opencv");
        // 如果 Makefile 未生成，重新执行 cmake
        let cmake_needed = !target.join("Makefile").is_file();
        // 如果执行了 cmake 或安装目录不存在，需要 make
        let install_needed = cmake_needed || !target.join("install").is_dir();
        // 工具链
        let path_with_musl_gcc = join_path_env(&[musl.join("bin")]);
        //
        if cmake_needed {
            dir::clear(&target).unwrap();
            // ffmpeg 路径
            let ffmpeg = self.0.target().join("ffmpeg").join("install").join("lib");
            // 创建平台相关 cmake
            let platform_cmake = self.0.target().join("musl-gcc.toolchain.cmake");
            fs::write(&platform_cmake, self.opencv_cmake(&ffmpeg)).unwrap();
            // 执行
            let mut cmake = Ext::new("cmake");
            if ffmpeg.is_dir() {
                cmake.env(
                    "PKG_CONFIG_LIBDIR",
                    ffmpeg.join("pkgconfig").canonicalize().unwrap(),
                );
            }
            cmake
                .current_dir(&target)
                .arg(format!(
                    "-DCMAKE_TOOLCHAIN_FILE={}",
                    platform_cmake.canonicalize().unwrap().display()
                ))
                .arg("-DWITH_FFMPEG=ON")
                .arg("-DCMAKE_BUILD_TYPE=Release")
                .arg(format!(
                    "-DCMAKE_INSTALL_PREFIX={}",
                    target.canonicalize().unwrap().join("install").display(),
                ))
                .arg(source)
                .env("PATH", &path_with_musl_gcc)
                .invoke();
        }
        //
        if install_needed {
            Make::install()
                .current_dir(&target)
                .j(num_cpus::get().min(8)) // 不能用太多线程，以免爆内存
                .env("PATH", path_with_musl_gcc)
                .invoke();
        }
        // 拷贝
        self.put_libs(musl, target.join("install"));
    }

    /// 构造一个用于 opencv 构建的 cmake 文件。
    fn opencv_cmake(&self, ffmpeg: impl AsRef<Path>) -> String {
        // 不会写 cmake
        if !matches!(self.0, Arch::Riscv64) {
            todo!();
        }
        const HEAD: &str = "\
set(CMAKE_SYSTEM_NAME      \"Linux\")
set(CMAKE_SYSTEM_PROCESSOR \"riscv64\")

set(CMAKE_C_COMPILER   riscv64-linux-musl-gcc)
set(CMAKE_CXX_COMPILER riscv64-linux-musl-g++)

set(CMAKE_C_FLAGS   \"\" CACHE STRING \"\")
set(CMAKE_CXX_FLAGS \"\" CACHE STRING \"\")

set(CMAKE_C_FLAGS   \"-march=rv64gc ${CMAKE_C_FLAGS}   ${CMAKE_PASS_TEST_FLAGS}\")
set(CMAKE_CXX_FLAGS \"-march=rv64gc ${CMAKE_CXX_FLAGS} ${CMAKE_PASS_TEST_FLAGS}\")";

        let ffmpeg = ffmpeg.as_ref();
        if ffmpeg.is_dir() {
            format!(
                "\
{HEAD}

set(CMAKE_LD_FFMPEG_FLAGS  \"-Wl,-rpath-link,{}\")
set(CMAKE_EXE_LINKER_FLAGS \"${{CMAKE_EXE_LINKER_FLAGS}} ${{CMAKE_LD_FFMPEG_FLAGS}}\")",
                ffmpeg.canonicalize().unwrap().display()
            )
        } else {
            HEAD.into()
        }
    }
}
#[cfg(test)]
mod tests {
    use super::super::LinuxRootfs;
    use crate::Arch;
    use std::fs;
    use std::path::PathBuf;

    fn scratch(what: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "eclipse-opencv-{what}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn cmake_of(ffmpeg: &std::path::Path) -> String {
        LinuxRootfs::new(Arch::Riscv64).opencv_cmake(ffmpeg)
    }

    /// The toolchain file is the only thing that tells cmake it is NOT building
    /// for the machine it runs on. The compilers it names are the ones
    /// `linux_musl_cross` downloads, and the triple is spelled out here while
    /// `Arch::name` holds the same fact: `put_ffmpeg` already passes
    /// `--cross-prefix=riscv64-linux-musl-` from a third copy of it. Tie them
    /// together, or a renamed arch leaves this file pointing at a compiler
    /// that is not in the tarball and cmake falls back to the host's `cc`,
    /// which produces a native OpenCV that the kernel cannot run.
    #[test]
    fn the_toolchain_file_cross_compiles_with_the_musl_tools_the_build_downloads() {
        let text = cmake_of(&PathBuf::from("/no/such/ffmpeg"));
        let arch = Arch::Riscv64.name();
        assert!(
            text.contains(&format!("set(CMAKE_C_COMPILER   {arch}-linux-musl-gcc)")),
            "the C compiler has to be the cross one: {text}"
        );
        assert!(
            text.contains(&format!("set(CMAKE_CXX_COMPILER {arch}-linux-musl-g++)")),
            "and OpenCV is mostly C++, so the C++ one as well: {text}"
        );
        assert!(text.contains("set(CMAKE_SYSTEM_NAME      \"Linux\")"));
        assert!(
            text.contains(&format!("set(CMAKE_SYSTEM_PROCESSOR \"{arch}\")")),
            "CMAKE_SYSTEM_NAME alone does not make it a cross build: {text}"
        );
    }

    /// Both compilers get the ISA string, and both get it on top of an
    /// explicitly EMPTIED cache variable. The emptying is the part that
    /// matters: `CMAKE_C_FLAGS` is a cache variable, so a second cmake run in
    /// a directory that already has a CMakeCache.txt would otherwise keep
    /// whatever the first one left and append `-march=rv64gc` again and again.
    #[test]
    fn both_compilers_get_the_isa_on_top_of_a_cleared_cache_variable() {
        let text = cmake_of(&PathBuf::from("/no/such/ffmpeg"));
        for flags in ["CMAKE_C_FLAGS  ", "CMAKE_CXX_FLAGS"] {
            let cleared = text
                .find(&format!("set({flags} \"\" CACHE STRING \"\")"))
                .unwrap_or_else(|| panic!("{flags} must be cleared first: {text}"));
            let set = text
                .find(&format!("set({flags} \"-march=rv64gc"))
                .unwrap_or_else(|| panic!("{flags} must carry the ISA: {text}"));
            assert!(
                cleared < set,
                "{flags} is cleared before it is appended to, or a re-run doubles it"
            );
        }
    }

    /// The rpath-link block exists for one reason: OpenCV links against an
    /// ffmpeg that was just built into the build tree, not an installed one,
    /// so the linker needs to be told where its transitive libraries are. With
    /// no ffmpeg build to point at, the flag must not be written at all -- a
    /// `-Wl,-rpath-link,` naming a directory that does not exist makes every
    /// link in the tree warn, and `WITH_FFMPEG=ON` would be a lie either way.
    #[test]
    fn the_ffmpeg_rpath_is_written_only_when_there_is_an_ffmpeg_build_to_point_at() {
        let d = scratch("rpath");
        let built = d.join("ffmpeg/install/lib");
        fs::create_dir_all(&built).unwrap();

        let with = cmake_of(&built);
        let flag = format!(
            "-Wl,-rpath-link,{}",
            built.canonicalize().unwrap().display()
        );
        assert!(with.contains(&flag), "{with}");
        assert!(
            with.contains("set(CMAKE_EXE_LINKER_FLAGS \"${CMAKE_EXE_LINKER_FLAGS} ${CMAKE_LD_FFMPEG_FLAGS}\")"),
            "the linker flags keep what cmake already put there: {with}"
        );

        let without = cmake_of(&d.join("ffmpeg/install/lib-not-built"));
        assert!(
            !without.contains("rpath-link"),
            "no ffmpeg build means no rpath at all: {without}"
        );
        assert!(
            !without.contains("CMAKE_EXE_LINKER_FLAGS"),
            "and nothing that would overwrite the linker flags: {without}"
        );
        assert!(
            with.starts_with(&without),
            "the ffmpeg block is added to the toolchain file, not a different one"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// The rpath is canonicalised, not copied through as spelled. cmake runs
    /// with the build directory as its working directory and the linker
    /// resolves `-rpath-link` from wherever it is invoked, so a path carrying
    /// `..` -- which is how it arrives, `self.0.target().join("ffmpeg")...` --
    /// is one directory rename away from pointing somewhere else entirely.
    #[test]
    fn the_rpath_is_canonical_and_not_however_the_caller_spelled_it() {
        let d = scratch("rpath-dots");
        fs::create_dir_all(d.join("lib")).unwrap();
        // `install` has to exist too: the kernel resolves `..` after following
        // what precedes it, so a stat through a missing directory fails.
        fs::create_dir_all(d.join("install")).unwrap();
        let text = cmake_of(&d.join("install").join("..").join("lib"));
        let line = text
            .lines()
            .find(|l| l.contains("rpath-link"))
            .unwrap_or_else(|| panic!("a real directory must get an rpath: {text}"));
        let path = line
            .split("-rpath-link,")
            .nth(1)
            .unwrap()
            .trim_end_matches(['"', ')']);
        assert!(
            std::path::Path::new(path).is_absolute(),
            "the rpath must not depend on cmake's working directory: {line}"
        );
        assert!(
            !path.contains(".."),
            "and must not depend on the directory it points through: {line}"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// There is no toolchain file for the other two architectures, and saying
    /// so with a panic is the honest answer: a silently native build would
    /// produce an OpenCV the kernel loads and cannot execute.
    #[test]
    #[should_panic]
    fn the_other_architectures_have_no_toolchain_file_and_say_so() {
        let _ = LinuxRootfs::new(Arch::X86_64).opencv_cmake("/no/such/ffmpeg");
    }
}
