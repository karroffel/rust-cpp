//! This crate is the `cpp` cargo build script implementation. It is useless
//! without the companion crates `cpp`, and `cpp_macro`.
//!
//! For more information, see the [`cpp` crate module level
//! documentation](https://docs.rs/cpp).

extern crate cpp_common;

extern crate cpp_synom as synom;

extern crate cpp_syn as syn;

extern crate cpp_synmap;

extern crate gcc;

#[macro_use]
extern crate lazy_static;

use std::env;
use std::path::{Path, PathBuf};
use std::ffi::OsString;
use std::fs::{remove_dir_all, create_dir, File};
use std::io::prelude::*;
use std::process::Command;
use syn::visit::Visitor;
use syn::{Mac, Span, Spanned, DUMMY_SPAN};
use cpp_common::{parsing, Closure, ClosureSig, Capture, Macro};
use cpp_synmap::SourceMap;

fn warnln_impl(a: String) {
    for s in a.lines() {
        println!("cargo:warning={}", s);
    }
}

macro_rules! warnln {
    ($($all:tt)*) => {
        $crate::warnln_impl(format!($($all)*));
    }
}

const LIB_NAME: &'static str = "librust_cpp_generated.a";
const MSVC_LIB_NAME: &'static str = "rust_cpp_generated.lib";

const INTERNAL_CPP_STRUCTS: &'static str = r#"
namespace rustcpp {

typedef unsigned long long usize;

struct Size {
    const char *name;
    usize *sizes;
    usize sizes_len;
};

template<typename T>
struct AlignOf {
    struct Inner {
        char a;
        T b;
    };
    static const unsigned long long value = sizeof(Inner) - sizeof(T);
};

} // namespace rustcpp
"#;

const SIZES_SCRIPT_MAIN: &'static str = r#"
#include <stdio.h>
#include <string.h>

extern rustcpp::Size __cpp_sizes[];

int main() {{
    for (rustcpp::usize i = 0; __cpp_sizes[i].name; ++i) {{
        printf("%s", __cpp_sizes[i].name);
        rustcpp::usize *sizes = __cpp_sizes[i].sizes;
        for (rustcpp::usize j = 0; j < __cpp_sizes[i].sizes_len; ++j) {{
            printf(" %llu", sizes[j]);
        }}
        printf(";\n");
    }}
}}
"#;

lazy_static! {
    static ref OUT_DIR: PathBuf =
        PathBuf::from(env::var("OUT_DIR").expect(r#"
-- rust-cpp fatal error --

The OUT_DIR environment variable was not set.
NOTE: rust-cpp's build function must be run in a build script."#));

    static ref TARGET: String =
        env::var("TARGET").expect(r#"
-- rust-cpp fatal error --

The TARGET environment variable was not set.
NOTE: rust-cpp's build function must be run in a build script."#);

    static ref CPP_DIR: PathBuf = OUT_DIR.join("rust_cpp");
}

fn gen_cpp_lib(visitor: &Handle) -> PathBuf {
    let result_path = CPP_DIR.join("cpp_closures.cpp");
    let mut output = File::create(&result_path)
        .expect("Unable to generate temporary C++ file");

    write!(output, "{}", INTERNAL_CPP_STRUCTS).unwrap();

    write!(output, "{}\n\n", &visitor.snippets).unwrap();

    let mut closures = String::new();
    for &Closure { ref body, ref sig } in &visitor.closures {
        let &ClosureSig { ref captures, ref cpp, .. } = sig;

        let name = sig.extern_name();

        // Generate the sizes array with the sizes of each of the argument types
        let mut sizes = vec![];
        if cpp != "void" {
            sizes.push(format!("sizeof({0}), rustcpp::AlignOf<{0}>::value", cpp));
        } else {
            sizes.push("0, 1".to_string());
        }
        for &Capture { ref cpp, .. } in captures {
            sizes.push(format!("sizeof({0}), rustcpp::AlignOf<{0}>::value", cpp));
        }

        closures.push_str(&format!(r#"
{{
    {0:?},
    {0}_sizes,
    sizeof({0}_sizes) / sizeof(rustcpp::usize),
}},"#,
                                   name.as_ref()));

        write!(output,
               r#"
rustcpp::usize {}_sizes[] = {{ {} }};
"#,
               name,
               sizes.join(", ")).unwrap();

        // Generate the parameters and function declaration
        let params = captures.iter()
            .map(|&Capture { mutable, ref name, ref cpp }| if mutable {
                format!("{} & {}", cpp, name)
            } else {
                format!("const {} & {}", cpp, name)
            })
            .collect::<Vec<_>>()
            .join(", ");

        write!(output,
               r#"
extern "C" {{
{} {}({}) {{
{}
}}
}}
"#,
               cpp,
               &name,
               params,
               body.node).unwrap();
    }

    write!(output,
           r#"
rustcpp::Size __cpp_sizes[] = {{ {} {{ 0 }} }};
"#,
           closures).unwrap();

    result_path
}

fn gen_sizes_cpp() -> PathBuf {
    let sizes_cpp = CPP_DIR.join("sizes.cpp");
    let mut sizes = File::create(&sizes_cpp).expect("Could not create cpp for computing sizes");
    write!(sizes, "{}{}", INTERNAL_CPP_STRUCTS, SIZES_SCRIPT_MAIN).unwrap();
    sizes_cpp
}

fn msvc_path(cmd: &mut Command, pre: &str, path: &Path) {
    let mut s = OsString::from(pre);
    s.push(path);
    cmd.arg(s);
}

fn gen_sizes_exe(config: &gcc::Config) {
    let sizes_cpp = gen_sizes_cpp();
    let sizes_exec = CPP_DIR.join("print_sizes");

    // Build the cpp_sizes executable file, which is used by the build plugin.
    let mut cmd = config.get_compiler().to_command();
    if TARGET.contains("msvc") {
        msvc_path(&mut cmd, "/Fo", &CPP_DIR.join("sizes.obj"));
        cmd.arg(&sizes_cpp);
        cmd.arg(OUT_DIR.join(MSVC_LIB_NAME));
        cmd.arg("/link");
        msvc_path(&mut cmd, "/OUT:", &sizes_exec);
        msvc_path(&mut cmd, "/FDB:", &CPP_DIR.join("sizes.pdb"));
    } else {
        cmd.arg("-o").arg(&sizes_exec);
        cmd.arg(&sizes_cpp);
        cmd.arg(OUT_DIR.join(LIB_NAME));
    }

    let output = cmd.output().expect("Unable to execute compiler to build sizes script");
    println!("{}", String::from_utf8_lossy(&output.stdout));
    warnln!("{}", String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        panic!("Compiler for building sizes exited with non-zero status code");
    }
}

fn clean_artifacts() {
    if CPP_DIR.is_dir() {
        remove_dir_all(&*CPP_DIR).expect(r#"
-- rust-cpp fatal error --

Failed to remove existing build artifacts from output directory."#);
    }

    create_dir(&*CPP_DIR).expect(r#"
-- rust-cpp fatal error --

Failed to create output object directory."#);
}

/// This struct is for advanced users of the build script. It allows providing
/// configuration options to `cpp` and the compiler when it is used to build.
///
/// ## API Note
///
/// Internally, `cpp` uses `gcc-rs` to build the compilation artifact, and many
/// of the methods defined on this type directly proxy to an internal
/// `gcc::Config` object.
pub struct Config {
    gcc: gcc::Config,
}

impl Config {
    /// Create a new `Config` object. This object will hold the configuration
    /// options which control the build. If you don't need to make any changes,
    /// `cpp_build::build` is a wrapper function around this interface.
    pub fn new() -> Config {
        let mut gcc = gcc::Config::new();
        gcc.cpp(true);
        Config {
            gcc: gcc,
        }
    }

    /// Add a directory to the `-I` or include path for headers
    pub fn include<P: AsRef<Path>>(&mut self, dir: P) -> &mut Self {
        self.gcc.include(dir);
        self
    }

    /// Specify a `-D` variable with an optional value
    pub fn define(&mut self, var: &str, val: Option<&str>) -> &mut Self {
        self.gcc.define(var, val);
        self
    }

    // XXX: Make sure that this works with sizes logic
    /// Add an arbitrary object file to link in
    pub fn object<P: AsRef<Path>>(&mut self, obj: P) -> &mut Self {
        self.gcc.object(obj);
        self
    }

    /// Add an arbitrary flag to the invocation of the compiler
    pub fn flag(&mut self, flag: &str) -> &mut Self {
        self.gcc.flag(flag);
        self
    }

    // XXX: Make sure this works with sizes logic
    /// Add a file which will be compiled
    pub fn file<P: AsRef<Path>>(&mut self, p: P) -> &mut Self {
        self.gcc.file(p);
        self
    }

    /// Set the standard library to link against when compiling with C++
    /// support.
    ///
    /// The default value of this property depends on the current target: On
    /// OS X `Some("c++")` is used, when compiling for a Visual Studio based
    /// target `None` is used and for other targets `Some("stdc++")` is used.
    ///
    /// A value of `None` indicates that no automatic linking should happen,
    /// otherwise cargo will link against the specified library.
    ///
    /// The given library name must not contain the `lib` prefix.
    pub fn cpp_link_stdlib(&mut self, cpp_link_stdlib: Option<&str>) -> &mut Self {
        self.gcc.cpp_link_stdlib(cpp_link_stdlib);
        self
    }

    /// Force the C++ compiler to use the specified standard library.
    ///
    /// Setting this option will automatically set `cpp_link_stdlib` to the same
    /// value.
    ///
    /// The default value of this option is always `None`.
    ///
    /// This option has no effect when compiling for a Visual Studio based
    /// target.
    ///
    /// This option sets the `-stdlib` flag, which is only supported by some
    /// compilers (clang, icc) but not by others (gcc). The library will not
    /// detect which compiler is used, as such it is the responsibility of the
    /// caller to ensure that this option is only used in conjuction with a
    /// compiler which supports the `-stdlib` flag.
    ///
    /// A value of `None` indicates that no specific C++ standard library should
    /// be used, otherwise `-stdlib` is added to the compile invocation.
    ///
    /// The given library name must not contain the `lib` prefix.
    pub fn cpp_set_stdlib(&mut self, cpp_set_stdlib: Option<&str>) -> &mut Self {
        self.gcc.cpp_set_stdlib(cpp_set_stdlib);
        self
    }

    // XXX: Add support for custom targets
    //
    // /// Configures the target this configuration will be compiling for.
    // ///
    // /// This option is automatically scraped from the `TARGET` environment
    // /// variable by build scripts, so it's not required to call this function.
    // pub fn target(&mut self, target: &str) -> &mut Self {
    //     self.gcc.target(target);
    //     self
    // }

    /// Configures the host assumed by this configuration.
    ///
    /// This option is automatically scraped from the `HOST` environment
    /// variable by build scripts, so it's not required to call this function.
    pub fn host(&mut self, host: &str) -> &mut Self {
        self.gcc.host(host);
        self
    }

    /// Configures the optimization level of the generated object files.
    ///
    /// This option is automatically scraped from the `OPT_LEVEL` environment
    /// variable by build scripts, so it's not required to call this function.
    pub fn opt_level(&mut self, opt_level: u32) -> &mut Self {
        self.gcc.opt_level(opt_level);
        self
    }

    /// Configures the optimization level of the generated object files.
    ///
    /// This option is automatically scraped from the `OPT_LEVEL` environment
    /// variable by build scripts, so it's not required to call this function.
    pub fn opt_level_str(&mut self, opt_level: &str) -> &mut Self {
        self.gcc.opt_level_str(opt_level);
        self
    }

    /// Configures whether the compiler will emit debug information when
    /// generating object files.
    ///
    /// This option is automatically scraped from the `PROFILE` environment
    /// variable by build scripts (only enabled when the profile is "debug"), so
    /// it's not required to call this function.
    pub fn debug(&mut self, debug: bool) -> &mut Self {
        self.gcc.debug(debug);
        self
    }

    // XXX: Add support for custom out_dir
    //
    // /// Configures the output directory where all object files and static
    // /// libraries will be located.
    // ///
    // /// This option is automatically scraped from the `OUT_DIR` environment
    // /// variable by build scripts, so it's not required to call this function.
    // pub fn out_dir<P: AsRef<Path>>(&mut self, out_dir: P) -> &mut Self {
    //     self.gcc.out_dir(out_dir);
    //     self
    // }

    /// Configures the compiler to be used to produce output.
    ///
    /// This option is automatically determined from the target platform or a
    /// number of environment variables, so it's not required to call this
    /// function.
    pub fn compiler<P: AsRef<Path>>(&mut self, compiler: P) -> &mut Self {
        self.gcc.compiler(compiler);
        self
    }

    /// Configures the tool used to assemble archives.
    ///
    /// This option is automatically determined from the target platform or a
    /// number of environment variables, so it's not required to call this
    /// function.
    pub fn archiver<P: AsRef<Path>>(&mut self, archiver: P) -> &mut Self {
        self.gcc.archiver(archiver);
        self
    }

    /// Define whether metadata should be emitted for cargo allowing it to
    /// automatically link the binary. Defaults to `true`.
    pub fn cargo_metadata(&mut self, cargo_metadata: bool) -> &mut Self {
        // XXX: Use this to control the cargo metadata which rust-cpp produces
        self.gcc.cargo_metadata(cargo_metadata);
        self
    }

    /// Configures whether the compiler will emit position independent code.
    ///
    /// This option defaults to `false` for `i686` and `windows-gnu` targets and
    /// to `true` for all other targets.
    pub fn pic(&mut self, pic: bool) -> &mut Self {
        self.gcc.pic(pic);
        self
    }

    /// Extracts `cpp` declarations from the passed-in crate root, and builds
    /// the associated static library to be linked in to the final binary.
    ///
    /// This method does not perform rust codegen - that is performed by `cpp`
    /// and `cpp_macros`, which perform the actual procedural macro expansion.
    ///
    /// This method may technically be called more than once for ergonomic
    /// reasons, but that usually won't do what you want. Use a different
    /// `Config` object each time you want to build a crate.
    pub fn build<P: AsRef<Path>>(&mut self, crate_root: P) {
        // Clean up any leftover artifacts
        clean_artifacts();

        // Parse the crate
        let mut sm = SourceMap::new();
        let krate = match sm.add_crate_root(crate_root) {
            Ok(krate) => krate,
            Err(err) => {
                warnln!(r#"-- rust-cpp parse error --

There was an error parsing the crate for the rust-cpp build script:

{}

In order to provide a better error message, the build script will exit
successfully, such that rustc can provide an error message."#, err);
                return;
            }
        };

        // Parse the macro definitions
        let mut visitor = Handle {
            closures: Vec::new(),
            snippets: String::new(),
            sm: &sm,
        };
        visitor.visit_crate(&krate);

        // Generate the C++ library code
        let filename = gen_cpp_lib(&visitor);

        // Build the C++ library using gcc-rs
        self.gcc.file(filename).compile(LIB_NAME);

        // Build the sizes executable which will be run by the macro
        gen_sizes_exe(&self.gcc);
    }
}

/// Run the `cpp` build process on the crate with a root at the given path.
/// Intended to be used within `build.rs` files.
pub fn build<P: AsRef<Path>>(path: P) {
    Config::new().build(path)
}

struct Handle<'a> {
    closures: Vec<Closure>,
    snippets: String,
    sm: &'a SourceMap,
}

fn extract_with_span(mut spanned: &mut Spanned<String>,
                     src: &str,
                     offset: usize,
                     sm: &SourceMap) {
    if spanned.span != DUMMY_SPAN {
        let src_slice = &src[spanned.span.lo..spanned.span.hi];
        spanned.span.lo += offset;
        spanned.span.hi += offset;

        let loc = sm.locinfo(spanned.span).unwrap();
        spanned.node = format!("#line {} {:?}\n",
                               loc.line, loc.path);
        for _ in 0..loc.col {
            spanned.node.push(' ');
        }
        spanned.node.push_str(src_slice);
    }
}

impl<'a> Visitor for Handle<'a> {
    fn visit_mac(&mut self, mac: &Mac) {
        if mac.path.segments.len() != 1 {
            return;
        }
        if mac.path.segments[0].ident.as_ref() == "cpp" {
            let tts = &mac.tts;
            assert!(tts.len() >= 1);
            let span = Span {
                lo: tts[0].span().lo,
                hi: tts[tts.len() - 1].span().hi,
            };
            let src = self.sm.source_text(span).unwrap();
            let input = synom::ParseState::new(&src);
            match parsing::build_macro(input).expect("cpp! macro") {
                Macro::Closure(mut c) => {
                    extract_with_span(&mut c.body, &src, span.lo, self.sm);
                    self.closures.push(c);
                }
                Macro::Lit(mut l) => {
                    extract_with_span(&mut l, &src, span.lo, self.sm);
                    self.snippets.push('\n');
                    self.snippets.push_str(&l.node);
                }
            }
        }
    }
}
