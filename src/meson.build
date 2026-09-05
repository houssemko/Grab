cargo = find_program('cargo', required: true)

cargo_options = ['--offline', '--release', '--path', meson.project_source_root()]

# Fetched cargo sources must be vendored for Flatpak (see build-aux/).
cargo_build = custom_target('cargo-build',
  build_by_default: true,
  build_always_stale: true,
  output: 'grab',
  console: true,
  install: true,
  install_dir: get_option('bindir'),
  command: [
    find_program('cargo.sh'), meson.project_source_root(), '@OUTPUT@',
    get_option('buildtype') == 'release' ? 'release' : 'debug',
  ],
)
