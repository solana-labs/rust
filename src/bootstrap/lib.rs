//! Implementation of rustbuild, the Rust build system.
//!
//! This module, and its descendants, are the implementation of the Rust build
//! system. Most of this build system is backed by Cargo but the outer layer
//! here serves as the ability to orchestrate calling Cargo, sequencing Cargo
//! builds, building artifacts like LLVM, etc. The goals of rustbuild are:
//!
//! * To be an easily understandable, easily extensible, and maintainable build
//!   system.
//! * Leverage standard tools in the Rust ecosystem to build the compiler, aka
//!   crates.io and Cargo.
//! * A standard interface to build across all platforms, including MSVC
//!
//! ## Architecture
//!
//! The build system defers most of the complicated logic managing invocations
//! of rustc and rustdoc to Cargo itself. However, moving through various stages
//! and copying artifacts is still necessary for it to do. Each time rustbuild
//! is invoked, it will iterate through the list of predefined steps and execute
//! each serially in turn if it matches the paths passed or is a default rule.
//! For each step rustbuild relies on the step internally being incremental and
//! parallel. Note, though, that the `-j` parameter to rustbuild gets forwarded
//! to appropriate test harnesses and such.
//!
//! Most of the "meaty" steps that matter are backed by Cargo, which does indeed
//! have its own parallelism and incremental management. Later steps, like
//! tests, aren't incremental and simply run the entire suite currently.
//! However, compiletest itself tries to avoid running tests when the artifacts
//! that are involved (mainly the compiler) haven't changed.
//!
//! When you execute `x.py build`, the steps executed are:
//!
//! * First, the python script is run. This will automatically download the
//!   stage0 rustc and cargo according to `src/stage0.txt`, or use the cached
//!   versions if they're available. These are then used to compile rustbuild
//!   itself (using Cargo). Finally, control is then transferred to rustbuild.
//!
//! * Rustbuild takes over, performs sanity checks, probes the environment,
//!   reads configuration, and starts executing steps as it reads the command
//!   line arguments (paths) or going through the default rules.
//!
//!   The build output will be something like the following:
//!
//!   Building stage0 std artifacts
//!   Copying stage0 std
//!   Building stage0 test artifacts
//!   Copying stage0 test
//!   Building stage0 compiler artifacts
//!   Copying stage0 rustc
//!   Assembling stage1 compiler
//!   Building stage1 std artifacts
//!   Copying stage1 std
//!   Building stage1 test artifacts
//!   Copying stage1 test
//!   Building stage1 compiler artifacts
//!   Copying stage1 rustc
//!   Assembling stage2 compiler
//!   Uplifting stage1 std
//!   Uplifting stage1 test
//!   Uplifting stage1 rustc
//!
//! Let's disect that a little:
//!
//! ## Building stage0 {std,test,compiler} artifacts
//!
//! These steps use the provided (downloaded, usually) compiler to compile the
//! local Rust source into libraries we can use.
//!
//! ## Copying stage0 {std,test,rustc}
//!
//! This copies the build output from Cargo into
//! `build/$HOST/stage0-sysroot/lib/rustlib/$ARCH/lib`. FIXME: this step's
//! documentation should be expanded -- the information already here may be
//! incorrect.
//!
//! ## Assembling stage1 compiler
//!
//! This copies the libraries we built in "building stage0 ... artifacts" into
//! the stage1 compiler's lib directory. These are the host libraries that the
//! compiler itself uses to run. These aren't actually used by artifacts the new
//! compiler generates. This step also copies the rustc and rustdoc binaries we
//! generated into build/$HOST/stage/bin.
//!
//! The stage1/bin/rustc is a fully functional compiler, but it doesn't yet have
//! any libraries to link built binaries or libraries to. The next 3 steps will
//! provide those libraries for it; they are mostly equivalent to constructing
//! the stage1/bin compiler so we don't go through them individually.
//!
//! ## Uplifting stage1 {std,test,rustc}
//!
//! This step copies the libraries from the stage1 compiler sysroot into the
//! stage2 compiler. This is done to avoid rebuilding the compiler; libraries
//! we'd build in this step should be identical (in function, if not necessarily
//! identical on disk) so there's no need to recompile the compiler again. Note
//! that if you want to, you can enable the full-bootstrap option to change this
//! behavior.
//!
//! Each step is driven by a separate Cargo project and rustbuild orchestrates
//! copying files between steps and otherwise preparing for Cargo to run.
//!
//! ## Further information
//!
//! More documentation can be found in each respective module below, and you can
//! also check out the `src/bootstrap/README.md` file for more information.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::slice;
use std::str;

#[cfg(unix)]
use std::os::unix::fs::symlink as symlink_file;
#[cfg(windows)]
use std::os::windows::fs::symlink_file;

use build_helper::{mtime, output, run, run_suppressed, t, try_run, try_run_suppressed};
use filetime::FileTime;

use crate::config::{LlvmLibunwind, TargetSelection};
use crate::util::{exe, libdir, CiEnv};

mod builder;
mod cache;
mod cc_detect;
mod channel;
mod check;
mod clean;
mod compile;
mod config;
mod dist;
mod doc;
mod flags;
mod format;
mod install;
mod metadata;
mod native;
mod run;
mod sanity;
mod setup;
mod tarball;
mod test;
mod tool;
mod toolstate;
pub mod util;

#[cfg(windows)]
mod job;

#[cfg(all(unix, not(target_os = "haiku")))]
mod job {
    pub unsafe fn setup(build: &mut crate::Build) {
        if build.config.low_priority {
            libc::setpriority(libc::PRIO_PGRP as _, 0, 10);
        }
    }
}

#[cfg(any(target_os = "haiku", target_os = "hermit", not(any(unix, windows))))]
mod job {
    pub unsafe fn setup(_build: &mut crate::Build) {}
}

use crate::cache::{Interned, INTERNER};
pub use crate::config::Config;
pub use crate::flags::Subcommand;

const LLVM_TOOLS: &[&str] = &[
    "llvm-cov",      // used to generate coverage report
    "llvm-nm",       // used to inspect binaries; it shows symbol names, their sizes and visibility
    "llvm-objcopy",  // used to transform ELFs into binary format which flashing tools consume
    "llvm-objdump",  // used to disassemble programs
    "llvm-profdata", // used to inspect and merge files generated by profiles
    "llvm-readobj",  // used to get information from ELFs/objects that the other tools don't provide
    "llvm-size",     // used to prints the size of the linker sections of a program
    "llvm-strip",    // used to discard symbols from binary files to reduce their size
    "llvm-ar",       // used for creating and modifying archive files
    "llvm-as",       // used to convert LLVM assembly to LLVM bitcode
    "llvm-dis",      // used to disassemble LLVM bitcode
    "llc",           // used to compile LLVM bytecode
    "opt",           // used to optimize LLVM bytecode
];

pub const VERSION: usize = 2;

/// A structure representing a Rust compiler.
///
/// Each compiler has a `stage` that it is associated with and a `host` that
/// corresponds to the platform the compiler runs on. This structure is used as
/// a parameter to many methods below.
#[derive(Eq, PartialOrd, Ord, PartialEq, Clone, Copy, Hash, Debug)]
pub struct Compiler {
    stage: u32,
    host: TargetSelection,
}

#[derive(PartialEq, Eq, Copy, Clone, Debug)]
pub enum DocTests {
    /// Run normal tests and doc tests (default).
    Yes,
    /// Do not run any doc tests.
    No,
    /// Only run doc tests.
    Only,
}

pub enum GitRepo {
    Rustc,
    Llvm,
}

/// Global configuration for the build system.
///
/// This structure transitively contains all configuration for the build system.
/// All filesystem-encoded configuration is in `config`, all flags are in
/// `flags`, and then parsed or probed information is listed in the keys below.
///
/// This structure is a parameter of almost all methods in the build system,
/// although most functions are implemented as free functions rather than
/// methods specifically on this structure itself (to make it easier to
/// organize).
pub struct Build {
    /// User-specified configuration from `config.toml`.
    config: Config,

    // Version information
    version: String,

    // Properties derived from the above configuration
    src: PathBuf,
    out: PathBuf,
    rust_info: channel::GitInfo,
    cargo_info: channel::GitInfo,
    rls_info: channel::GitInfo,
    rust_analyzer_info: channel::GitInfo,
    clippy_info: channel::GitInfo,
    miri_info: channel::GitInfo,
    rustfmt_info: channel::GitInfo,
    in_tree_llvm_info: channel::GitInfo,
    local_rebuild: bool,
    fail_fast: bool,
    doc_tests: DocTests,
    verbosity: usize,

    // Targets for which to build
    build: TargetSelection,
    hosts: Vec<TargetSelection>,
    targets: Vec<TargetSelection>,

    // Stage 0 (downloaded) compiler, lld and cargo or their local rust equivalents
    initial_rustc: PathBuf,
    initial_cargo: PathBuf,
    initial_lld: PathBuf,
    initial_libdir: PathBuf,

    // Runtime state filled in later on
    // C/C++ compilers and archiver for all targets
    cc: HashMap<TargetSelection, cc::Tool>,
    cxx: HashMap<TargetSelection, cc::Tool>,
    ar: HashMap<TargetSelection, PathBuf>,
    ranlib: HashMap<TargetSelection, PathBuf>,
    // Miscellaneous
    crates: HashMap<Interned<String>, Crate>,
    is_sudo: bool,
    ci_env: CiEnv,
    delayed_failures: RefCell<Vec<String>>,
    prerelease_version: Cell<Option<u32>>,
    tool_artifacts:
        RefCell<HashMap<TargetSelection, HashMap<String, (&'static str, PathBuf, Vec<String>)>>>,
}

#[derive(Debug)]
struct Crate {
    name: Interned<String>,
    deps: HashSet<Interned<String>>,
    id: String,
    path: PathBuf,
}

impl Crate {
    fn local_path(&self, build: &Build) -> PathBuf {
        self.path.strip_prefix(&build.config.src).unwrap().into()
    }
}

/// When building Rust various objects are handled differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DependencyType {
    /// Libraries originating from proc-macros.
    Host,
    /// Typical Rust libraries.
    Target,
    /// Non Rust libraries and objects shipped to ease usage of certain targets.
    TargetSelfContained,
}

/// The various "modes" of invoking Cargo.
///
/// These entries currently correspond to the various output directories of the
/// build system, with each mod generating output in a different directory.
#[derive(Debug, Hash, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mode {
    /// Build the standard library, placing output in the "stageN-std" directory.
    Std,

    /// Build librustc, and compiler libraries, placing output in the "stageN-rustc" directory.
    Rustc,

    /// Build a codegen backend for rustc, placing the output in the "stageN-codegen" directory.
    Codegen,

    /// Build a tool, placing output in the "stage0-bootstrap-tools"
    /// directory. This is for miscellaneous sets of tools that are built
    /// using the bootstrap stage0 compiler in its entirety (target libraries
    /// and all). Typically these tools compile with stable Rust.
    ToolBootstrap,

    /// Build a tool which uses the locally built std, placing output in the
    /// "stageN-tools" directory. Its usage is quite rare, mainly used by
    /// compiletest which needs libtest.
    ToolStd,

    /// Build a tool which uses the locally built rustc and the target std,
    /// placing the output in the "stageN-tools" directory. This is used for
    /// anything that needs a fully functional rustc, such as rustdoc, clippy,
    /// cargo, rls, rustfmt, miri, etc.
    ToolRustc,
}

impl Mode {
    pub fn is_tool(&self) -> bool {
        matches!(self, Mode::ToolBootstrap | Mode::ToolRustc | Mode::ToolStd)
    }

    pub fn must_support_dlopen(&self) -> bool {
        matches!(self, Mode::Std | Mode::Codegen)
    }
}

impl Build {
    /// Creates a new set of build configuration from the `flags` on the command
    /// line and the filesystem `config`.
    ///
    /// By default all build output will be placed in the current directory.
    pub fn new(config: Config) -> Build {
        let src = config.src.clone();
        let out = config.out.clone();

        let is_sudo = match env::var_os("SUDO_USER") {
            Some(sudo_user) => match env::var_os("USER") {
                Some(user) => user != sudo_user,
                None => false,
            },
            None => false,
        };

        let ignore_git = config.ignore_git;
        let rust_info = channel::GitInfo::new(ignore_git, &src);
        let cargo_info = channel::GitInfo::new(ignore_git, &src.join("src/tools/cargo"));
        let rls_info = channel::GitInfo::new(ignore_git, &src.join("src/tools/rls"));
        let rust_analyzer_info =
            channel::GitInfo::new(ignore_git, &src.join("src/tools/rust-analyzer"));
        let clippy_info = channel::GitInfo::new(ignore_git, &src.join("src/tools/clippy"));
        let miri_info = channel::GitInfo::new(ignore_git, &src.join("src/tools/miri"));
        let rustfmt_info = channel::GitInfo::new(ignore_git, &src.join("src/tools/rustfmt"));

        // we always try to use git for LLVM builds
        let in_tree_llvm_info = channel::GitInfo::new(false, &src.join("src/llvm-project"));

        let initial_target_libdir_str = if config.dry_run {
            "/dummy/lib/path/to/lib/".to_string()
        } else {
            output(
                Command::new(&config.initial_rustc)
                    .arg("--target")
                    .arg(config.build.rustc_target_arg())
                    .arg("--print")
                    .arg("target-libdir"),
            )
        };
        let initial_target_dir = Path::new(&initial_target_libdir_str).parent().unwrap();
        let initial_lld = initial_target_dir.join("bin").join("rust-lld");

        let initial_sysroot = if config.dry_run {
            "/dummy".to_string()
        } else {
            output(Command::new(&config.initial_rustc).arg("--print").arg("sysroot"))
        };
        let initial_libdir = initial_target_dir
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .strip_prefix(initial_sysroot.trim())
            .unwrap()
            .to_path_buf();

        let version = std::fs::read_to_string(src.join("src").join("version"))
            .expect("failed to read src/version");
        let version = version.trim();

        let mut build = Build {
            initial_rustc: config.initial_rustc.clone(),
            initial_cargo: config.initial_cargo.clone(),
            initial_lld,
            initial_libdir,
            local_rebuild: config.local_rebuild,
            fail_fast: config.cmd.fail_fast(),
            doc_tests: config.cmd.doc_tests(),
            verbosity: config.verbose,

            build: config.build,
            hosts: config.hosts.clone(),
            targets: config.targets.clone(),

            config,
            version: version.to_string(),
            src,
            out,

            rust_info,
            cargo_info,
            rls_info,
            rust_analyzer_info,
            clippy_info,
            miri_info,
            rustfmt_info,
            in_tree_llvm_info,
            cc: HashMap::new(),
            cxx: HashMap::new(),
            ar: HashMap::new(),
            ranlib: HashMap::new(),
            crates: HashMap::new(),
            is_sudo,
            ci_env: CiEnv::current(),
            delayed_failures: RefCell::new(Vec::new()),
            prerelease_version: Cell::new(None),
            tool_artifacts: Default::default(),
        };

        build.verbose("finding compilers");
        cc_detect::find(&mut build);
        build.verbose("running sanity check");
        sanity::check(&mut build);

        // If local-rust is the same major.minor as the current version, then force a
        // local-rebuild
        let local_version_verbose =
            output(Command::new(&build.initial_rustc).arg("--version").arg("--verbose"));
        let local_release = local_version_verbose
            .lines()
            .filter_map(|x| x.strip_prefix("release:"))
            .next()
            .unwrap()
            .trim();
        if local_release.split('.').take(2).eq(version.split('.').take(2)) {
            build.verbose(&format!("auto-detected local-rebuild {}", local_release));
            build.local_rebuild = true;
        }

        build.verbose("learning about cargo");
        metadata::build(&mut build);

        build
    }

    pub fn build_triple(&self) -> &[Interned<String>] {
        slice::from_ref(&self.build.triple)
    }

    /// Executes the entire build, as configured by the flags and configuration.
    pub fn build(&mut self) {
        unsafe {
            job::setup(self);
        }

        if let Subcommand::Format { check } = self.config.cmd {
            return format::format(self, check);
        }

        if let Subcommand::Clean { all } = self.config.cmd {
            return clean::clean(self, all);
        }

        if let Subcommand::Setup { profile } = &self.config.cmd {
            return setup::setup(&self.config.src, *profile);
        }

        {
            let builder = builder::Builder::new(&self);
            if let Some(path) = builder.paths.get(0) {
                if path == Path::new("nonexistent/path/to/trigger/cargo/metadata") {
                    return;
                }
            }
        }

        if !self.config.dry_run {
            {
                self.config.dry_run = true;
                let builder = builder::Builder::new(&self);
                builder.execute_cli();
            }
            self.config.dry_run = false;
            let builder = builder::Builder::new(&self);
            builder.execute_cli();
        } else {
            let builder = builder::Builder::new(&self);
            builder.execute_cli();
        }

        // Check for postponed failures from `test --no-fail-fast`.
        let failures = self.delayed_failures.borrow();
        if failures.len() > 0 {
            println!("\n{} command(s) did not execute successfully:\n", failures.len());
            for failure in failures.iter() {
                println!("  - {}\n", failure);
            }
            process::exit(1);
        }
    }

    /// Clear out `dir` if `input` is newer.
    ///
    /// After this executes, it will also ensure that `dir` exists.
    fn clear_if_dirty(&self, dir: &Path, input: &Path) -> bool {
        let stamp = dir.join(".stamp");
        let mut cleared = false;
        if mtime(&stamp) < mtime(input) {
            self.verbose(&format!("Dirty - {}", dir.display()));
            let _ = fs::remove_dir_all(dir);
            cleared = true;
        } else if stamp.exists() {
            return cleared;
        }
        t!(fs::create_dir_all(dir));
        t!(File::create(stamp));
        cleared
    }

    /// Gets the space-separated set of activated features for the standard
    /// library.
    fn std_features(&self, target: TargetSelection) -> String {
        let mut features = "panic-unwind".to_string();

        match self.config.llvm_libunwind.unwrap_or_default() {
            LlvmLibunwind::InTree => features.push_str(" llvm-libunwind"),
            LlvmLibunwind::System => features.push_str(" system-llvm-libunwind"),
            LlvmLibunwind::No => {}
        }
        if self.config.backtrace {
            features.push_str(" backtrace");
        }
        if self.config.profiler_enabled(target) {
            features.push_str(" profiler");
        }
        features
    }

    /// Gets the space-separated set of activated features for the compiler.
    fn rustc_features(&self) -> String {
        let mut features = String::new();
        if self.config.jemalloc {
            features.push_str("jemalloc");
        }
        if self.config.llvm_enabled() {
            features.push_str(" llvm");
        }

        // If debug logging is on, then we want the default for tracing:
        // https://github.com/tokio-rs/tracing/blob/3dd5c03d907afdf2c39444a29931833335171554/tracing/src/level_filters.rs#L26
        // which is everything (including debug/trace/etc.)
        // if its unset, if debug_assertions is on, then debug_logging will also be on
        // as well as tracing *ignoring* this feature when debug_assertions is on
        if !self.config.rust_debug_logging {
            features.push_str(" max_level_info");
        }

        features
    }

    /// Component directory that Cargo will produce output into (e.g.
    /// release/debug)
    fn cargo_dir(&self) -> &'static str {
        if self.config.rust_optimize { "release" } else { "debug" }
    }

    fn tools_dir(&self, compiler: Compiler) -> PathBuf {
        let out = self
            .out
            .join(&*compiler.host.triple)
            .join(format!("stage{}-tools-bin", compiler.stage));
        t!(fs::create_dir_all(&out));
        out
    }

    /// Returns the root directory for all output generated in a particular
    /// stage when running with a particular host compiler.
    ///
    /// The mode indicates what the root directory is for.
    fn stage_out(&self, compiler: Compiler, mode: Mode) -> PathBuf {
        let suffix = match mode {
            Mode::Std => "-std",
            Mode::Rustc => "-rustc",
            Mode::Codegen => "-codegen",
            Mode::ToolBootstrap => "-bootstrap-tools",
            Mode::ToolStd | Mode::ToolRustc => "-tools",
        };
        self.out.join(&*compiler.host.triple).join(format!("stage{}{}", compiler.stage, suffix))
    }

    /// Returns the root output directory for all Cargo output in a given stage,
    /// running a particular compiler, whether or not we're building the
    /// standard library, and targeting the specified architecture.
    fn cargo_out(&self, compiler: Compiler, mode: Mode, target: TargetSelection) -> PathBuf {
        self.stage_out(compiler, mode).join(&*target.triple).join(self.cargo_dir())
    }

    /// Root output directory for LLVM compiled for `target`
    ///
    /// Note that if LLVM is configured externally then the directory returned
    /// will likely be empty.
    fn llvm_out(&self, target: TargetSelection) -> PathBuf {
        self.out.join(&*target.triple).join("llvm")
    }

    fn lld_out(&self, target: TargetSelection) -> PathBuf {
        self.out.join(&*target.triple).join("lld")
    }

    /// Output directory for all documentation for a target
    fn doc_out(&self, target: TargetSelection) -> PathBuf {
        self.out.join(&*target.triple).join("doc")
    }

    /// Output directory for all documentation for a target
    fn compiler_doc_out(&self, target: TargetSelection) -> PathBuf {
        self.out.join(&*target.triple).join("compiler-doc")
    }

    /// Output directory for some generated md crate documentation for a target (temporary)
    fn md_doc_out(&self, target: TargetSelection) -> Interned<PathBuf> {
        INTERNER.intern_path(self.out.join(&*target.triple).join("md-doc"))
    }

    /// Returns `true` if no custom `llvm-config` is set for the specified target.
    ///
    /// If no custom `llvm-config` was specified then Rust's llvm will be used.
    fn is_rust_llvm(&self, target: TargetSelection) -> bool {
        if self.config.llvm_from_ci && target == self.config.build {
            return true;
        }

        match self.config.target_config.get(&target) {
            Some(ref c) => c.llvm_config.is_none(),
            None => true,
        }
    }

    /// Returns the path to llvm/bin
    fn llvm_bin(&self, target: TargetSelection) -> PathBuf {
        let target_config = self.config.target_config.get(&target);
        if let Some(s) = target_config.and_then(|c| c.llvm_config.as_ref()) {
            let llvm_bindir = output(Command::new(s).arg("--bindir"));
            PathBuf::from(llvm_bindir.trim())
        } else {
            self.llvm_out(self.config.build).join("bin")
        }
    }

    /// Returns the path to `FileCheck` binary for the specified target
    fn llvm_filecheck(&self, target: TargetSelection) -> PathBuf {
        let target_config = self.config.target_config.get(&target);
        if let Some(s) = target_config.and_then(|c| c.llvm_filecheck.as_ref()) {
            s.to_path_buf()
        } else if let Some(s) = target_config.and_then(|c| c.llvm_config.as_ref()) {
            let llvm_bindir = output(Command::new(s).arg("--bindir"));
            let filecheck = Path::new(llvm_bindir.trim()).join(exe("FileCheck", target));
            if filecheck.exists() {
                filecheck
            } else {
                // On Fedora the system LLVM installs FileCheck in the
                // llvm subdirectory of the libdir.
                let llvm_libdir = output(Command::new(s).arg("--libdir"));
                let lib_filecheck =
                    Path::new(llvm_libdir.trim()).join("llvm").join(exe("FileCheck", target));
                if lib_filecheck.exists() {
                    lib_filecheck
                } else {
                    // Return the most normal file name, even though
                    // it doesn't exist, so that any error message
                    // refers to that.
                    filecheck
                }
            }
        } else {
            let base = self.llvm_out(self.config.build).join("build");
            let base = if !self.ninja() && self.config.build.contains("msvc") {
                if self.config.llvm_optimize {
                    if self.config.llvm_release_debuginfo {
                        base.join("RelWithDebInfo")
                    } else {
                        base.join("Release")
                    }
                } else {
                    base.join("Debug")
                }
            } else {
                base
            };
            base.join("bin").join(exe("FileCheck", target))
        }
    }

    /// Directory for libraries built from C/C++ code and shared between stages.
    fn native_dir(&self, target: TargetSelection) -> PathBuf {
        self.out.join(&*target.triple).join("native")
    }

    /// Root output directory for rust_test_helpers library compiled for
    /// `target`
    fn test_helpers_out(&self, target: TargetSelection) -> PathBuf {
        self.native_dir(target).join("rust-test-helpers")
    }

    /// Adds the `RUST_TEST_THREADS` env var if necessary
    fn add_rust_test_threads(&self, cmd: &mut Command) {
        if env::var_os("RUST_TEST_THREADS").is_none() {
            cmd.env("RUST_TEST_THREADS", self.jobs().to_string());
        }
    }

    /// Returns the libdir of the snapshot compiler.
    fn rustc_snapshot_libdir(&self) -> PathBuf {
        self.rustc_snapshot_sysroot().join(libdir(self.config.build))
    }

    /// Returns the sysroot of the snapshot compiler.
    fn rustc_snapshot_sysroot(&self) -> &Path {
        self.initial_rustc.parent().unwrap().parent().unwrap()
    }

    /// Runs a command, printing out nice contextual information if it fails.
    fn run(&self, cmd: &mut Command) {
        if self.config.dry_run {
            return;
        }
        self.verbose(&format!("running: {:?}", cmd));
        run(cmd)
    }

    /// Runs a command, printing out nice contextual information if it fails.
    fn run_quiet(&self, cmd: &mut Command) {
        if self.config.dry_run {
            return;
        }
        self.verbose(&format!("running: {:?}", cmd));
        run_suppressed(cmd)
    }

    /// Runs a command, printing out nice contextual information if it fails.
    /// Exits if the command failed to execute at all, otherwise returns its
    /// `status.success()`.
    fn try_run(&self, cmd: &mut Command) -> bool {
        if self.config.dry_run {
            return true;
        }
        self.verbose(&format!("running: {:?}", cmd));
        try_run(cmd)
    }

    /// Runs a command, printing out nice contextual information if it fails.
    /// Exits if the command failed to execute at all, otherwise returns its
    /// `status.success()`.
    fn try_run_quiet(&self, cmd: &mut Command) -> bool {
        if self.config.dry_run {
            return true;
        }
        self.verbose(&format!("running: {:?}", cmd));
        try_run_suppressed(cmd)
    }

    pub fn is_verbose(&self) -> bool {
        self.verbosity > 0
    }

    /// Prints a message if this build is configured in verbose mode.
    fn verbose(&self, msg: &str) {
        if self.is_verbose() {
            println!("{}", msg);
        }
    }

    pub fn is_verbose_than(&self, level: usize) -> bool {
        self.verbosity > level
    }

    /// Prints a message if this build is configured in more verbose mode than `level`.
    fn verbose_than(&self, level: usize, msg: &str) {
        if self.is_verbose_than(level) {
            println!("{}", msg);
        }
    }

    fn info(&self, msg: &str) {
        if self.config.dry_run {
            return;
        }
        println!("{}", msg);
    }

    /// Returns the number of parallel jobs that have been configured for this
    /// build.
    fn jobs(&self) -> u32 {
        self.config.jobs.unwrap_or_else(|| num_cpus::get() as u32)
    }

    fn debuginfo_map_to(&self, which: GitRepo) -> Option<String> {
        if !self.config.rust_remap_debuginfo {
            return None;
        }

        match which {
            GitRepo::Rustc => {
                let sha = self.rust_sha().unwrap_or(&self.version);
                Some(format!("/rustc/{}", sha))
            }
            GitRepo::Llvm => Some(String::from("/rustc/llvm")),
        }
    }

    /// Returns the path to the C compiler for the target specified.
    fn cc(&self, target: TargetSelection) -> &Path {
        self.cc[&target].path()
    }

    /// Returns a list of flags to pass to the C compiler for the target
    /// specified.
    fn cflags(&self, target: TargetSelection, which: GitRepo) -> Vec<String> {
        // Filter out -O and /O (the optimization flags) that we picked up from
        // cc-rs because the build scripts will determine that for themselves.
        let mut base = self.cc[&target]
            .args()
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.starts_with("-O") && !s.starts_with("/O"))
            .collect::<Vec<String>>();

        // If we're compiling on macOS then we add a few unconditional flags
        // indicating that we want libc++ (more filled out than libstdc++) and
        // we want to compile for 10.7. This way we can ensure that
        // LLVM/etc are all properly compiled.
        if target.contains("apple-darwin") {
            base.push("-stdlib=libc++".into());
        }

        // Work around an apparently bad MinGW / GCC optimization,
        // See: http://lists.llvm.org/pipermail/cfe-dev/2016-December/051980.html
        // See: https://gcc.gnu.org/bugzilla/show_bug.cgi?id=78936
        if &*target.triple == "i686-pc-windows-gnu" {
            base.push("-fno-omit-frame-pointer".into());
        }

        if let Some(map_to) = self.debuginfo_map_to(which) {
            let map = format!("{}={}", self.src.display(), map_to);
            let cc = self.cc(target);
            if cc.ends_with("clang") || cc.ends_with("gcc") {
                base.push(format!("-fdebug-prefix-map={}", map));
            } else if cc.ends_with("clang-cl.exe") {
                base.push("-Xclang".into());
                base.push(format!("-fdebug-prefix-map={}", map));
            }
        }
        base
    }

    /// Returns the path to the `ar` archive utility for the target specified.
    fn ar(&self, target: TargetSelection) -> Option<&Path> {
        self.ar.get(&target).map(|p| &**p)
    }

    /// Returns the path to the `ranlib` utility for the target specified.
    fn ranlib(&self, target: TargetSelection) -> Option<&Path> {
        self.ranlib.get(&target).map(|p| &**p)
    }

    /// Returns the path to the C++ compiler for the target specified.
    fn cxx(&self, target: TargetSelection) -> Result<&Path, String> {
        match self.cxx.get(&target) {
            Some(p) => Ok(p.path()),
            None => {
                Err(format!("target `{}` is not configured as a host, only as a target", target))
            }
        }
    }

    /// Returns the path to the linker for the given target if it needs to be overridden.
    fn linker(&self, target: TargetSelection) -> Option<&Path> {
        if let Some(linker) = self.config.target_config.get(&target).and_then(|c| c.linker.as_ref())
        {
            Some(linker)
        } else if target.contains("vxworks") {
            // need to use CXX compiler as linker to resolve the exception functions
            // that are only existed in CXX libraries
            Some(self.cxx[&target].path())
        } else if target != self.config.build
            && util::use_host_linker(target)
            && !target.contains("msvc")
        {
            Some(self.cc(target))
        } else if self.config.use_lld && !self.is_fuse_ld_lld(target) && self.build == target {
            Some(&self.initial_lld)
        } else {
            None
        }
    }

    // LLD is used through `-fuse-ld=lld` rather than directly.
    // Only MSVC targets use LLD directly at the moment.
    fn is_fuse_ld_lld(&self, target: TargetSelection) -> bool {
        self.config.use_lld && !target.contains("msvc")
    }

    /// Returns if this target should statically link the C runtime, if specified
    fn crt_static(&self, target: TargetSelection) -> Option<bool> {
        if target.contains("pc-windows-msvc") {
            Some(true)
        } else {
            self.config.target_config.get(&target).and_then(|t| t.crt_static)
        }
    }

    /// Returns the "musl root" for this `target`, if defined
    fn musl_root(&self, target: TargetSelection) -> Option<&Path> {
        self.config
            .target_config
            .get(&target)
            .and_then(|t| t.musl_root.as_ref())
            .or_else(|| self.config.musl_root.as_ref())
            .map(|p| &**p)
    }

    /// Returns the "musl libdir" for this `target`.
    fn musl_libdir(&self, target: TargetSelection) -> Option<PathBuf> {
        let t = self.config.target_config.get(&target)?;
        if let libdir @ Some(_) = &t.musl_libdir {
            return libdir.clone();
        }
        self.musl_root(target).map(|root| root.join("lib"))
    }

    /// Returns the sysroot for the wasi target, if defined
    fn wasi_root(&self, target: TargetSelection) -> Option<&Path> {
        self.config.target_config.get(&target).and_then(|t| t.wasi_root.as_ref()).map(|p| &**p)
    }

    /// Returns `true` if this is a no-std `target`, if defined
    fn no_std(&self, target: TargetSelection) -> Option<bool> {
        self.config.target_config.get(&target).map(|t| t.no_std)
    }

    /// Returns `true` if the target will be tested using the `remote-test-client`
    /// and `remote-test-server` binaries.
    fn remote_tested(&self, target: TargetSelection) -> bool {
        self.qemu_rootfs(target).is_some()
            || target.contains("android")
            || env::var_os("TEST_DEVICE_ADDR").is_some()
    }

    /// Returns the root of the "rootfs" image that this target will be using,
    /// if one was configured.
    ///
    /// If `Some` is returned then that means that tests for this target are
    /// emulated with QEMU and binaries will need to be shipped to the emulator.
    fn qemu_rootfs(&self, target: TargetSelection) -> Option<&Path> {
        self.config.target_config.get(&target).and_then(|t| t.qemu_rootfs.as_ref()).map(|p| &**p)
    }

    /// Path to the python interpreter to use
    fn python(&self) -> &Path {
        self.config.python.as_ref().unwrap()
    }

    /// Temporary directory that extended error information is emitted to.
    fn extended_error_dir(&self) -> PathBuf {
        self.out.join("tmp/extended-error-metadata")
    }

    /// Tests whether the `compiler` compiling for `target` should be forced to
    /// use a stage1 compiler instead.
    ///
    /// Currently, by default, the build system does not perform a "full
    /// bootstrap" by default where we compile the compiler three times.
    /// Instead, we compile the compiler two times. The final stage (stage2)
    /// just copies the libraries from the previous stage, which is what this
    /// method detects.
    ///
    /// Here we return `true` if:
    ///
    /// * The build isn't performing a full bootstrap
    /// * The `compiler` is in the final stage, 2
    /// * We're not cross-compiling, so the artifacts are already available in
    ///   stage1
    ///
    /// When all of these conditions are met the build will lift artifacts from
    /// the previous stage forward.
    fn force_use_stage1(&self, compiler: Compiler, target: TargetSelection) -> bool {
        !self.config.full_bootstrap
            && compiler.stage >= 2
            && (self.hosts.iter().any(|h| *h == target) || target == self.build)
    }

    /// Given `num` in the form "a.b.c" return a "release string" which
    /// describes the release version number.
    ///
    /// For example on nightly this returns "a.b.c-nightly", on beta it returns
    /// "a.b.c-beta.1" and on stable it just returns "a.b.c".
    fn release(&self, num: &str) -> String {
        match &self.config.channel[..] {
            "stable" => num.to_string(),
            "beta" => {
                if self.rust_info.is_git() {
                    format!("{}-beta.{}", num, self.beta_prerelease_version())
                } else {
                    format!("{}-beta", num)
                }
            }
            "nightly" => format!("{}-nightly", num),
            _ => format!("{}-dev", num),
        }
    }

    fn beta_prerelease_version(&self) -> u32 {
        if let Some(s) = self.prerelease_version.get() {
            return s;
        }

        // Figure out how many merge commits happened since we branched off master.
        // That's our beta number!
        // (Note that we use a `..` range, not the `...` symmetric difference.)
        let count = output(
            Command::new("git")
                .arg("rev-list")
                .arg("--count")
                .arg("--merges")
                .arg("refs/remotes/origin/master..HEAD")
                .current_dir(&self.src),
        );
        let n = count.trim().parse().unwrap();
        self.prerelease_version.set(Some(n));
        n
    }

    /// Returns the value of `release` above for Rust itself.
    fn rust_release(&self) -> String {
        self.release(&self.version)
    }

    /// Returns the "package version" for a component given the `num` release
    /// number.
    ///
    /// The package version is typically what shows up in the names of tarballs.
    /// For channels like beta/nightly it's just the channel name, otherwise
    /// it's the `num` provided.
    fn package_vers(&self, num: &str) -> String {
        match &self.config.channel[..] {
            "stable" => num.to_string(),
            "beta" => "beta".to_string(),
            "nightly" => "nightly".to_string(),
            _ => format!("{}-dev", num),
        }
    }

    /// Returns the value of `package_vers` above for Rust itself.
    fn rust_package_vers(&self) -> String {
        self.package_vers(&self.version)
    }

    fn llvm_link_tools_dynamically(&self, target: TargetSelection) -> bool {
        target.contains("linux-gnu") || target.contains("apple-darwin")
    }

    /// Returns the `version` string associated with this compiler for Rust
    /// itself.
    ///
    /// Note that this is a descriptive string which includes the commit date,
    /// sha, version, etc.
    fn rust_version(&self) -> String {
        let mut version = self.rust_info.version(self, &self.version);
        if let Some(ref s) = self.config.description {
            version.push_str(" (");
            version.push_str(s);
            version.push_str(")");
        }
        version
    }

    /// Returns the full commit hash.
    fn rust_sha(&self) -> Option<&str> {
        self.rust_info.sha()
    }

    /// Returns the `a.b.c` version that the given package is at.
    fn release_num(&self, package: &str) -> String {
        let toml_file_name = self.src.join(&format!("src/tools/{}/Cargo.toml", package));
        let toml = t!(fs::read_to_string(&toml_file_name));
        for line in toml.lines() {
            if let Some(stripped) =
                line.strip_prefix("version = \"").and_then(|s| s.strip_suffix("\""))
            {
                return stripped.to_owned();
            }
        }

        panic!("failed to find version in {}'s Cargo.toml", package)
    }

    /// Returns `true` if unstable features should be enabled for the compiler
    /// we're building.
    fn unstable_features(&self) -> bool {
        match &self.config.channel[..] {
            "stable" | "beta" => false,
            "nightly" | _ => true,
        }
    }

    /// Returns a Vec of all the dependencies of the given root crate,
    /// including transitive dependencies and the root itself. Only includes
    /// "local" crates (those in the local source tree, not from a registry).
    fn in_tree_crates(&self, root: &str, target: Option<TargetSelection>) -> Vec<&Crate> {
        let mut ret = Vec::new();
        let mut list = vec![INTERNER.intern_str(root)];
        let mut visited = HashSet::new();
        while let Some(krate) = list.pop() {
            let krate = &self.crates[&krate];
            ret.push(krate);
            for dep in &krate.deps {
                if !self.crates.contains_key(dep) {
                    // Ignore non-workspace members.
                    continue;
                }
                // Don't include optional deps if their features are not
                // enabled. Ideally this would be computed from `cargo
                // metadata --features …`, but that is somewhat slow. Just
                // skip `build_helper` since there aren't any operations we
                // want to perform on it. In the future, we may want to
                // consider just filtering all build and dev dependencies in
                // metadata::build.
                if visited.insert(dep)
                    && dep != "build_helper"
                    && (dep != "profiler_builtins"
                        || target
                            .map(|t| self.config.profiler_enabled(t))
                            .unwrap_or(self.config.any_profiler_enabled()))
                    && (dep != "rustc_codegen_llvm" || self.config.llvm_enabled())
                {
                    list.push(*dep);
                }
            }
        }
        ret
    }

    fn read_stamp_file(&self, stamp: &Path) -> Vec<(PathBuf, DependencyType)> {
        if self.config.dry_run {
            return Vec::new();
        }

        let mut paths = Vec::new();
        let contents = t!(fs::read(stamp), &stamp);
        // This is the method we use for extracting paths from the stamp file passed to us. See
        // run_cargo for more information (in compile.rs).
        for part in contents.split(|b| *b == 0) {
            if part.is_empty() {
                continue;
            }
            let dependency_type = match part[0] as char {
                'h' => DependencyType::Host,
                's' => DependencyType::TargetSelfContained,
                't' => DependencyType::Target,
                _ => unreachable!(),
            };
            let path = PathBuf::from(t!(str::from_utf8(&part[1..])));
            paths.push((path, dependency_type));
        }
        paths
    }

    /// Copies a file from `src` to `dst`
    pub fn copy(&self, src: &Path, dst: &Path) {
        if self.config.dry_run {
            return;
        }
        self.verbose_than(1, &format!("Copy {:?} to {:?}", src, dst));
        if src == dst {
            return;
        }
        let _ = fs::remove_file(&dst);
        let metadata = t!(src.symlink_metadata());
        if metadata.file_type().is_symlink() {
            let link = t!(fs::read_link(src));
            t!(symlink_file(link, dst));
        } else if let Ok(()) = fs::hard_link(src, dst) {
            // Attempt to "easy copy" by creating a hard link
            // (symlinks don't work on windows), but if that fails
            // just fall back to a slow `copy` operation.
        } else {
            if let Err(e) = fs::copy(src, dst) {
                panic!("failed to copy `{}` to `{}`: {}", src.display(), dst.display(), e)
            }
            t!(fs::set_permissions(dst, metadata.permissions()));
            let atime = FileTime::from_last_access_time(&metadata);
            let mtime = FileTime::from_last_modification_time(&metadata);
            t!(filetime::set_file_times(dst, atime, mtime));
        }
    }

    /// Search-and-replaces within a file. (Not maximally efficiently: allocates a
    /// new string for each replacement.)
    pub fn replace_in_file(&self, path: &Path, replacements: &[(&str, &str)]) {
        if self.config.dry_run {
            return;
        }
        let mut contents = String::new();
        let mut file = t!(OpenOptions::new().read(true).write(true).open(path));
        t!(file.read_to_string(&mut contents));
        for &(target, replacement) in replacements {
            contents = contents.replace(target, replacement);
        }
        t!(file.seek(SeekFrom::Start(0)));
        t!(file.set_len(0));
        t!(file.write_all(contents.as_bytes()));
    }

    /// Copies the `src` directory recursively to `dst`. Both are assumed to exist
    /// when this function is called.
    pub fn cp_r(&self, src: &Path, dst: &Path) {
        if self.config.dry_run {
            return;
        }
        for f in self.read_dir(src) {
            let path = f.path();
            let name = path.file_name().unwrap();
            let dst = dst.join(name);
            if t!(f.file_type()).is_dir() {
                t!(fs::create_dir_all(&dst));
                self.cp_r(&path, &dst);
            } else {
                let _ = fs::remove_file(&dst);
                self.copy(&path, &dst);
            }
        }
    }

    /// Copies the `src` directory recursively to `dst`. Both are assumed to exist
    /// when this function is called. Unwanted files or directories can be skipped
    /// by returning `false` from the filter function.
    pub fn cp_filtered(&self, src: &Path, dst: &Path, filter: &dyn Fn(&Path) -> bool) {
        // Immediately recurse with an empty relative path
        self.recurse_(src, dst, Path::new(""), filter)
    }

    // Inner function does the actual work
    fn recurse_(&self, src: &Path, dst: &Path, relative: &Path, filter: &dyn Fn(&Path) -> bool) {
        for f in self.read_dir(src) {
            let path = f.path();
            let name = path.file_name().unwrap();
            let dst = dst.join(name);
            let relative = relative.join(name);
            // Only copy file or directory if the filter function returns true
            if filter(&relative) {
                if t!(f.file_type()).is_dir() {
                    let _ = fs::remove_dir_all(&dst);
                    self.create_dir(&dst);
                    self.recurse_(&path, &dst, &relative, filter);
                } else {
                    let _ = fs::remove_file(&dst);
                    self.copy(&path, &dst);
                }
            }
        }
    }

    fn copy_to_folder(&self, src: &Path, dest_folder: &Path) {
        let file_name = src.file_name().unwrap();
        let dest = dest_folder.join(file_name);
        self.copy(src, &dest);
    }

    fn install(&self, src: &Path, dstdir: &Path, perms: u32) {
        if self.config.dry_run {
            return;
        }
        let dst = dstdir.join(src.file_name().unwrap());
        self.verbose_than(1, &format!("Install {:?} to {:?}", src, dst));
        t!(fs::create_dir_all(dstdir));
        drop(fs::remove_file(&dst));
        {
            if !src.exists() {
                panic!("Error: File \"{}\" not found!", src.display());
            }
            let metadata = t!(src.symlink_metadata());
            if let Err(e) = fs::copy(&src, &dst) {
                panic!("failed to copy `{}` to `{}`: {}", src.display(), dst.display(), e)
            }
            t!(fs::set_permissions(&dst, metadata.permissions()));
            let atime = FileTime::from_last_access_time(&metadata);
            let mtime = FileTime::from_last_modification_time(&metadata);
            t!(filetime::set_file_times(&dst, atime, mtime));
        }
        chmod(&dst, perms);
    }

    fn create(&self, path: &Path, s: &str) {
        if self.config.dry_run {
            return;
        }
        t!(fs::write(path, s));
    }

    fn read(&self, path: &Path) -> String {
        if self.config.dry_run {
            return String::new();
        }
        t!(fs::read_to_string(path))
    }

    fn create_dir(&self, dir: &Path) {
        if self.config.dry_run {
            return;
        }
        t!(fs::create_dir_all(dir))
    }

    fn remove_dir(&self, dir: &Path) {
        if self.config.dry_run {
            return;
        }
        t!(fs::remove_dir_all(dir))
    }

    fn read_dir(&self, dir: &Path) -> impl Iterator<Item = fs::DirEntry> {
        let iter = match fs::read_dir(dir) {
            Ok(v) => v,
            Err(_) if self.config.dry_run => return vec![].into_iter(),
            Err(err) => panic!("could not read dir {:?}: {:?}", dir, err),
        };
        iter.map(|e| t!(e)).collect::<Vec<_>>().into_iter()
    }

    fn remove(&self, f: &Path) {
        if self.config.dry_run {
            return;
        }
        fs::remove_file(f).unwrap_or_else(|_| panic!("failed to remove {:?}", f));
    }

    /// Returns if config.ninja is enabled, and checks for ninja existence,
    /// exiting with a nicer error message if not.
    fn ninja(&self) -> bool {
        let mut cmd_finder = crate::sanity::Finder::new();

        if self.config.ninja_in_file {
            // Some Linux distros rename `ninja` to `ninja-build`.
            // CMake can work with either binary name.
            if cmd_finder.maybe_have("ninja-build").is_none()
                && cmd_finder.maybe_have("ninja").is_none()
            {
                eprintln!(
                    "
Couldn't find required command: ninja
You should install ninja, or set ninja=false in config.toml
"
                );
                std::process::exit(1);
            }
        }

        // If ninja isn't enabled but we're building for MSVC then we try
        // doubly hard to enable it. It was realized in #43767 that the msbuild
        // CMake generator for MSVC doesn't respect configuration options like
        // disabling LLVM assertions, which can often be quite important!
        //
        // In these cases we automatically enable Ninja if we find it in the
        // environment.
        if !self.config.ninja_in_file && self.config.build.contains("msvc") {
            if cmd_finder.maybe_have("ninja").is_some() {
                return true;
            }
        }

        self.config.ninja_in_file
    }
}

#[cfg(unix)]
fn chmod(path: &Path, perms: u32) {
    use std::os::unix::fs::*;
    t!(fs::set_permissions(path, fs::Permissions::from_mode(perms)));
}
#[cfg(windows)]
fn chmod(_path: &Path, _perms: u32) {}

impl Compiler {
    pub fn with_stage(mut self, stage: u32) -> Compiler {
        self.stage = stage;
        self
    }

    /// Returns `true` if this is a snapshot compiler for `build`'s configuration
    pub fn is_snapshot(&self, build: &Build) -> bool {
        self.stage == 0 && self.host == build.build
    }

    /// Returns if this compiler should be treated as a final stage one in the
    /// current build session.
    /// This takes into account whether we're performing a full bootstrap or
    /// not; don't directly compare the stage with `2`!
    pub fn is_final_stage(&self, build: &Build) -> bool {
        let final_stage = if build.config.full_bootstrap { 2 } else { 1 };
        self.stage >= final_stage
    }
}

fn envify(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '-' => '_',
            c => c,
        })
        .flat_map(|c| c.to_uppercase())
        .collect()
}
