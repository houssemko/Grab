//! Small GTK dialog helpers shared by windows that must not depend on
//! each other: leaf module (gtk/adw only) breaking the
//! `window ↔ install_help` import cycle.

use adw::prelude::*;
use gtk4::prelude::*;
use libadwaita as adw;

/// Close the dialog when the button is clicked (Cancel/close actions).
pub(crate) fn close_on_click(btn: &gtk4::Button, dialog: &adw::Dialog) {
    let weak = dialog.downgrade();
    btn.connect_clicked(move |_| {
        if let Some(d) = weak.upgrade() {
            d.close();
        }
    });
}
