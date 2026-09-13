fn main() {
    println!("cargo:rerun-if-changed=data/io.github.houssemko.Grab.gschema.xml");
    println!("cargo:rustc-env=GRAB_VERSION={}", env!("CARGO_PKG_VERSION"));
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
    // Installed message catalogs live under $prefix/share/locale; the
    // meson build passes it as GRAB_PREFIX, dev/test builds fall back to
    // the source po/ dir at runtime (no .mo there, so gettext is a no-op).
    let prefix = std::env::var("GRAB_PREFIX").unwrap_or_else(|_| "/usr".to_string());
    println!(
        "cargo:rustc-env=GRAB_LOCALEDIR={}/share/locale",
        prefix.trim_end_matches('/')
    );
}
