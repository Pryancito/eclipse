# Notes for developers (draft)

[Español](for-developers.md) · **English**

> Translated from Chinese from the original zCore (rCore-OS) notes. The
> toolchain version and the submodule list have been brought up to date; the
> list of unstable features is the one from that time.

This document shares some development decisions and points out things to keep
in mind.

## Contents

- [Toolchain policy](#toolchain-policy)
  > - When the tracked toolchain is updated
  > - Which unstable features are used and why
- [Code quality](#code-quality)
  > How to use `#[deny(...)]` and `#[allow(...)]`
- [Dependency policy](#dependency-policy)
  > When and how to add a dependency
- [features and cfg](#features-and-cfg)
  > How to use conditional compilation

## Toolchain policy

For the specific reasons detailed [below](#unstable-features-in-use), the
project depends on some unstable features and only builds with a nightly
toolchain. The repository records the tested toolchain in a
[toolchain file](../rust-toolchain.toml) cargo understands, so building with
cargo installs the right toolchain by itself. Today it is pinned to
`nightly-2026-09-01`.

Although the toolchain is nightly, it is updated following stable: every time a
new stable comes out, the default toolchain moves to the nightly closest to
that stable, roughly every 6 weeks. Look up on the
[release information site](https://forge.rust-lang.org/) when the latest stable
branched and use that date's nightly.

### Unstable features in use

#### [`doc_cfg`](https://doc.rust-lang.org/unstable-book/language-features/doc-cfg.html)

Modules: `zcore-drivers`, `kernel-hal`, `zcore-loader`

Marks in the documentation which platforms each item is available on.

It improves the documentation but does not affect usage; it can be removed if
needed.

#### [`naked_functions`](https://doc.rust-lang.org/unstable-book/language-features/naked-functions.html), [`asm_sym`](https://doc.rust-lang.org/unstable-book/language-features/asm-sym.html), [`asm_const`](https://doc.rust-lang.org/unstable-book/language-features/asm-const.html)

Module: `zcore`

All three support writing a naked function in Rust. A naked function inserts
no stack operations, so it can be used before the stack is set up. Together with
`asm_const`, which brings Rust constants into inline assembly, and `asm_sym`,
which brings in Rust symbols, it keeps as much of the boot phase as possible
under Rust's protection (no hand-written constants or exported global
symbols).

It can be removed, but that is not recommended.

#### [`default_alloc_error_handler`](https://doc.rust-lang.org/unstable-book/language-features/default-alloc-error-handler.html)

Module: `zcore`

Asks `alloc` for a default allocation-failure callback.

As soon as `no_std` and `alloc` are used together an allocation-failure
callback is needed: either the default one or a custom one through another
unstable feature,
[`alloc-error-handler`](https://doc.rust-lang.org/unstable-book/language-features/alloc-error-handler.html).

## Code quality

Code quality relies mainly on **clippy**, so the environment should make clippy
easy to run. There are three parts:

1. rust-analyzer integration

   If you develop with VS Code, rust-analyzer has a checkOnSave option that
   checks on every save (with how fast modern compilers are, that is
   feasible). With autosave, it checks all the time; just make clippy the
   default check.

   Of course clippy is slow; if your machine cannot keep up, turn it off.

2. `#![deny(warnings)]`

   This option treats warnings from the current module (and its submodules) as
   errors, which stops code with warnings from compiling and helps fix
   problems early. clippy produces warnings too, and they also count as
   errors.

   Today every module has this option (some forbid more things, such as
   unsafe_code or missing_docs, which are not covered here) and builds. While
   developing you can comment it out for convenience, but restore it before
   opening a PR.

3. GitHub Actions

   The clippy check is in the GitHub workflow and runs on every commit, which
   shows others your code quality is covered. Locally you can check with
   `cargo check-style`, which follows the same process and uses clippy the same
   way as the GitHub workflow.

## Dependency policy

From most particular to most general, dependencies take these 4 forms:

1. [git submodules](#git-submodules)
2. [cargo projects in personal repositories](#cargo-projects-in-personal-repositories)
3. [cargo projects in organisation repositories](#cargo-projects-in-organisation-repositories)
4. [crates published on crates.io](#crates-published-on-cratesio)

### git submodules

The most particular form; it is unavoidable for projects that do not use cargo
(because they are not written in Rust).

Today these are depended on this way:

- libc-test

  The musl libc tests, written in C.

- tests

  The test framework, mostly in Python.

- nvidia-rm-sys/vendor/open-gpu-kernel-modules

  NVIDIA's open kernel modules, where the RM comes from.

(`rboot`, the UEFI loader, used to be a submodule and now lives inside the
repository.)

### cargo projects in personal repositories

A fairly particular form. A project may depend directly on a personal project
if and only if one of these holds:

- it is experimental, unstable or may even be abandoned;
- it is stable, but does not fit crates.io and no organisation wants to take
  it in;
- it is a fork of an organisation's project with a PR submitted but not yet
  accepted;
- it is a fork of an organisation's project that, for good reasons, has been
  decided to stay separate forever as an independent project.

Where possible, these cases should be actively resolved.

The commit hash must be pinned.

### cargo projects in organisation repositories

The normal form, although where possible it is better to publish them on
crates.io.

The commit hash must be pinned.

### crates published on crates.io

The normal form; use them freely.

Try to track the latest version. If a dependency cannot be tracked, record why.

## features and cfg

zCore supports different hardware on several platforms, so build options are
hard to avoid. But misusing them can cause confusion or reduce test coverage.
So these rules are recommended when setting `#[cfg(...)]`:

1. What depends only on the platform uses target_arch;
2. what depends on other factors is better decided at run time, for example
   through the device tree;
3. what cannot be decided at run time can use a feature, but the reason must be
   explained at the project's entry point (lib.rs or main.rs);
4. a platform-specific feature should carry a constraint like this one, instead
   of using `all(target_arch = ..., feature = ...)`:

   ```rust
   #[cfg(all(feature = "sbi", not(target_arch = "riscv64")))]
   compile_error!("`sbi` is only available on RISC-V platforms");
   ```

> The current code does not fully follow these rules; it will be fixed over time.
