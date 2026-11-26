//! Compiles tests/c/*.c against include/wartlog.h and the built libwartlog.so, and runs them.

use std::path::PathBuf;
use std::process::Command;

fn target_dir() -> PathBuf {
    // tests run from target/<profile>/deps
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().parent().unwrap().to_path_buf()
}

/// `cargo test` does not rebuild the cdylib for integration tests, so build it here (once).
fn build_lib() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let release = target_dir().file_name().is_some_and(|n| n == "release");
        let mut c = Command::new(env!("CARGO"));
        c.args(["build", "-q", "-p", "wartlog-ffi", "--lib"]);
        if release {
            c.arg("--release");
        }
        assert!(c.status().unwrap().success(), "building libwartlog failed");
    });
}

fn compile_and_run(name: &str) {
    build_lib();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let lib_dir = target_dir();
    assert!(lib_dir.join("libwartlog.so").exists(), "libwartlog.so not built in {}", lib_dir.display());
    let out = tempfile::tempdir().unwrap();
    let exe = out.path().join(name);
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let st = Command::new(&cc)
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&exe)
        .arg(manifest.join("tests/c").join(format!("{name}.c")))
        .arg("-I")
        .arg(manifest.join("include"))
        .arg("-L")
        .arg(&lib_dir)
        .args(["-lwartlog"])
        .status()
        .expect("a C compiler (set CC)");
    assert!(st.success(), "compiling {name}.c failed");
    let data = tempfile::tempdir().unwrap();
    let o = Command::new(&exe).arg(data.path()).env("LD_LIBRARY_PATH", &lib_dir).output().unwrap();
    assert!(o.status.success(), "{name} failed:\n{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
}

#[test]
fn smoke() {
    compile_and_run("smoke");
}

#[test]
fn header_is_current() {
    // regenerating the header must not change it (skipped when cbindgen is not installed)
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tmp = tempfile::tempdir().unwrap();
    let fresh = tmp.path().join("wartlog.h");
    let r = Command::new("cbindgen").current_dir(&manifest).args(["--config", "cbindgen.toml", "--crate", "wartlog-ffi", "--output"]).arg(&fresh).output();
    match r {
        Ok(o) if o.status.success() => {
            let a = std::fs::read_to_string(manifest.join("include/wartlog.h")).unwrap();
            let b = std::fs::read_to_string(&fresh).unwrap();
            assert!(a == b, "include/wartlog.h is stale: run cbindgen --config cbindgen.toml --crate wartlog-ffi --output include/wartlog.h");
        }
        _ => eprintln!("cbindgen not available; header check skipped"),
    }
}
