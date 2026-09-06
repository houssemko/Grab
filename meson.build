project('grab', 'rust',
  version: '0.1.0',
  meson_version: '>= 0.59.0',
  license: 'MIT',
)

gnome = import('gnome')

cargo_sh = files('build-aux/cargo.sh')

subdir('data')
subdir('src')

gnome.post_install(
  glib_compile_schemas: true,
  gtk_update_icon_cache: true,
  update_desktop_database: true,
)
