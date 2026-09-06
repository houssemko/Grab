fn main() {
    println!("cargo:rerun-if-changed=data/io.github.houssemko.Grab.gschema.xml");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rustc-env=GRAB_VERSION={}", app_version());
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let schema_dir = out_dir.join("grab-schemas/glib-2.0/schemas");
    let _ = std::fs::create_dir_all(&schema_dir);

    let src = std::path::Path::new("data/io.github.houssemko.Grab.gschema.xml");
    if src.exists() {
        let dst = schema_dir.join("io.github.houssemko.Grab.gschema.xml");
        if std::fs::copy(src, &dst).is_ok() {
            let status = std::process::Command::new("glib-compile-schemas")
                .arg(&schema_dir)
                .status();
            if !matches!(status, Ok(s) if s.success()) {
                eprintln!("cargo:warning=glib-compile-schemas failed; run with GSETTINGS_SCHEMA_DIR pointing at compiled schemas");
            }
        }
    }
    println!("cargo:rustc-env=GRAB_SCHEMA_DIR={}", schema_dir.display());
}

fn app_version() -> String {
    std::process::Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().trim_start_matches('v').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}
