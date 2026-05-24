pub mod customization;
pub mod hud;
pub mod radial_menu;

use bevy::prelude::*;

pub struct UiPlugin;

impl Plugin for UiPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(customization::CustomizationPlugin)
            .init_resource::<radial_menu::RadialCursor>()
            .add_observer(radial_menu::on_open_radial)
            .add_systems(Startup, hud::spawn_hud)
            .add_systems(
                Update,
                (radial_menu::track_cursor, radial_menu::detect_release).chain(),
            );
    }
}
