//! Client binary. Runs the wisp game with full rendering + input. In
//! single-player mode (the current state) it just plays the game locally.
//! When `ClientNetPlugin` lands, this will connect to a server URL parsed
//! from a CLI flag (default 127.0.0.1:5000).

use bevy::prelude::*;
use bevy_inspector_egui::{bevy_egui::EguiPlugin, quick::WorldInspectorPlugin};

fn main() {
    let mut app = App::new();
    app.add_plugins((
        DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "wisp — client".to_string(),
                ..default()
            }),
            ..default()
        }),
        EguiPlugin::default(),
        WorldInspectorPlugin::new(),
    ));
    wisp::build_shared(&mut app);
    app.add_plugins(wisp::net::ClientNetPlugin);
    // After ClientPlugins are registered (inside ClientNetPlugin), wire
    // avian via lightyear so Position/Rotation sync follows the
    // replication path (Stage Q).
    wisp::add_avian_with_lightyear(&mut app);
    app.run();
}
