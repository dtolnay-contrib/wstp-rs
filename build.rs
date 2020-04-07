extern crate bindgen;

use cfg_if::cfg_if;

use std::env;
use std::path::PathBuf;
use std::process;

const WSTP_FRAMEWORK: &str = "Frameworks/wstp.framework/";
const WSTP_STATIC_ARCHIVE: &str =
    "SystemFiles/Links/WSTP/DeveloperKit/MacOSX-x86-64/CompilerAdditions/libWSTPi4.a";

fn main() {
    let installation = get_wolfram_installation();

    println!(
        "cargo:warning=info: Using Wolfram installation at: {}",
        installation.display()
    );

    generate_bindings(&installation);
    link_wstp_statically(&installation);
}

cfg_if![if #[cfg(all(target_os = "macos", target_arch = "x86_64"))] {
    fn link_wstp_statically(installation: &PathBuf) {
        let lib = installation.join(WSTP_STATIC_ARCHIVE);
        let lib = lib.to_str()
            .expect("could not convert WSTP archive path to str");
        let lib = lipo_native_library(lib);
        link_library_file(lib);
    }

    /// Use the macOS `lipo` command to construct an x86_64 archive file from the WSTPi4.a
    /// file in the Mathematica layout. This is necessary as a workaround to a bug in the
    /// Rust compiler at the moment: https://github.com/rust-lang/rust/issues/50220.
    /// The problem is that WSTPi4.a is a so called "universal binary"; it's an archive
    /// file with multiple copies of the same library, each for a different target
    /// architecture. The `lipo -thin` command creates a new archive which contains just
    /// the library for the named architecture.
    fn lipo_native_library(wstp_lib: &str) -> PathBuf {
        // Place the lipo'd library file in the system temporary directory.
        let output_lib = std::env::temp_dir().join("libWSTP-x86-64.a");
        let output_lib = output_lib.to_str()
            .expect("could not convert WSTP archive path to str");

        let output = process::Command::new("lipo")
            .args(&[wstp_lib, "-thin", "x86_64", "-output", output_lib])
            .output()
            .expect("failed to invoke macOS `lipo` command");

        if !output.status.success() {
            panic!("unable to lipo WSTP library: {:#?}", output);
        }

        PathBuf::from(output_lib)
    }
} else {
    // FIXME: Add support for Windows and Linux platforms.
    compile_error!("unsupported target platform");
}];

fn link_library_file(libfile: PathBuf) {
    let search_dir = libfile.parent().unwrap().display().to_string();

    let libname = libfile
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .trim_start_matches("lib");
    println!("cargo:rustc-link-search={}", search_dir);
    println!("cargo:rustc-link-lib=static={}", libname);
}

fn generate_bindings(installation: &PathBuf) {
    let header = installation.join(&*WSTP_FRAMEWORK).join("Headers/wstp.h");

    let bindings = bindgen::Builder::default()
        .clang_arg(format!(
            "-I/{}",
            installation
                .join(&*WSTP_FRAMEWORK)
                .join("Headers/")
                .display()
        ))
        .header(header.display().to_string())
        .generate_comments(true)
        // NOTE: At time of writing this will silently fail to work if you are using a
        //       nightly version of Rust, making the generated bindings almost impossible
        //       to decipher.
        //
        //       Instead, use `$ cargo doc --document-private-items && open target/doc` to
        //       have a look at the generated documentation, which is easier to read and
        //       navigate anyway.
        .rustfmt_bindings(true)
        .generate()
        .expect("unable to generate Rust bindings to WSTP using bindgen");

    let filename = "WSTP_bindings.rs";
    // OUT_DIR is set by cargo before running this build.rs file.
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join(filename);
    bindings
        .write_to_file(out_path)
        .expect("failed to write Rust bindings with IO error");
}

/// Evaluate `$InstallationDirectory` using wolframscript to get location of the
/// developers Mathematica installation.
///
/// TODO: Make this value settable using an environment variable; some people don't have
///       wolframscript, or they may have multiple Mathematica installations and will want
///       to be able to exactly specify which one to use. WOLFRAM_INSTALLATION_DIRECTORY.
fn get_wolfram_installation() -> PathBuf {
    let output: process::Output = process::Command::new("wolframscript")
        .args(&["-code", "$InstallationDirectory"])
        .output()
        .expect("unable to execute wolframscript command");

    if !output.status.success() {
        panic!(
            "wolframscript exited with non-success status code: {}",
            output.status
        );
    }

    let stdout = match String::from_utf8(output.stdout.clone()) {
        Ok(s) => s,
        Err(err) => {
            panic!(
                "wolframscript output is not valid UTF-8: {}: {}",
                err,
                String::from_utf8_lossy(&output.stdout)
            );
        }
    };

    let first_line = stdout
        .lines()
        .next()
        .expect("wolframscript output was empty");

    PathBuf::from(first_line)
}
