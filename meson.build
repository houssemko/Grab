project('grab', 'rust',
  version: '0.1.0',
  meson_version: '>= 0.59.0',
  license: 'GPL-3.0-or-later',
)

gnome = import('gnome')
i18n = import('i18n')

application_id = 'org.gnome.WgetFrontend'

subdir('data')
subdir('src')
subdir('po')

gnome.post_install(
  glib_compile_schemas: true,
  gtk_update_icon_cache: true,
  update_desktop_database: true,
)
