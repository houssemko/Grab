use super::*;

const MAGNET: &str = "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&dn=test+file";
const BARE_HASH: &str = "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3";

#[test]
fn classifies_magnets() {
    assert!(is_magnet("magnet:?xt=urn:btih:abc"));
    assert!(is_magnet("MAGNET:?xt=urn:btih:abc"));
    assert!(is_magnet("  magnet:?xt=urn:btih:abc"));
    assert!(!is_magnet("https://example.com/f.iso"));
    assert!(!is_magnet("magnetfoo"));
    assert!(!is_magnet(""));
}

#[test]
fn parses_valid_magnets() {
    let m = parse_magnet(MAGNET).unwrap();
    assert_eq!(m.name.as_deref(), Some("test file"));
    assert!(parse_magnet(BARE_HASH).is_ok());
    assert!(parse_magnet("magnet:?xt=urn:btih:xyz").is_err());
    assert!(parse_magnet("https://example.com/f.iso").is_err());
    assert!(parse_magnet("").is_err());
}

#[test]
fn classifier_never_panics_on_unicode() {
    assert!(!is_magnet("éééééééééé"));
    assert!(!is_magnet("é"));
    assert!(!is_magnet("magnet"));
}

#[test]
fn stub_prefers_name_then_hash() {
    assert_eq!(stub_name(MAGNET).as_deref(), Some("test file"));
    assert_eq!(
        stub_name(BARE_HASH).as_deref(),
        Some("a94a8fe5ccb19ba61c4c0873d391e987982fbbd3")
    );
    assert_eq!(stub_name("magnet:?xt=urn:btih:xyz"), None);
}

#[test]
fn multifile_torrents_get_name_subfolder() {
    let dest = std::path::PathBuf::from("/tmp/dl");
    // Multi-file torrents land in a subfolder named after the torrent…
    let out = output_folder_for(&dest, Some("Cosmos Laundromat".to_string()), true, "abc123");
    assert_eq!(out, std::path::PathBuf::from("/tmp/dl/Cosmos Laundromat"));
    // …single-file torrents sit flat even when a name is known…
    assert_eq!(
        output_folder_for(&dest, Some("movie.mp4".to_string()), false, "abc123"),
        dest
    );
    // …and hostile names fall back to the info-hash, never the parent.
    let out = output_folder_for(&dest, Some("../evil".to_string()), true, "abc123");
    assert_eq!(out, std::path::PathBuf::from("/tmp/dl/abc123"));
    assert!(out.starts_with(&dest));
    assert_eq!(
        output_folder_for(&dest, None, true, "abc123"),
        std::path::PathBuf::from("/tmp/dl/abc123")
    );
}
