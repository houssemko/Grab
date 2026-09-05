project('grab', 'rust',
  version: '0.1.0',
  meson_version: '>= 0.59.0',
  license: 'MIT',
)

gnome = import('gnome')
i18n = import('i18n')

application_id = 'io.github.houssemko.Grab'

cargo_sh = files('build-aux/cargo.sh')

subdir('data')
subdir('src')
subdir('po')

gnome.post_install(
  glib_compile_schemas: true,
  gtk_update_icon_cache: true,
  update_desktop_database: true,
)
