//! Compile the GSettings schema next to the binary so `cargo run` works
//! without a system install. Never fails the build: if
//! `glib-compile-schemas` is missing we just skip and rely on an installed
//! schema (Meson/Flatpak path).

fn main() {
    println!("cargo:rerun-if-changed=data/org.gnome.Grab.gschema.xml");
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let schema_dir = out_dir.join("wget-schemas/glib-2.0/schemas");
    let _ = std::fs::create_dir_all(&schema_dir);

    let src = std::path::Path::new("data/org.gnome.Grab.gschema.xml");
    if src.exists() {
        let dst = schema_dir.join("org.gnome.Grab.gschema.xml");
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
