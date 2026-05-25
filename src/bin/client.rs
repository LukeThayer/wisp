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
    // Override the server addr from `--ip` / `--port` if present.
    // `ClientNetPlugin::build` already inserted a `ConnectTo` carrying
    // the default addr + a generated client_id; preserve the id and
    // swap just the server field.
    let server_addr = wisp::net::parse_addr_args(wisp::net::default_server_addr());
    let client_id = app
        .world()
        .resource::<wisp::net::client::ConnectTo>()
        .client_id;
    app.insert_resource(wisp::net::client::ConnectTo {
        server: server_addr,
        client_id,
    });
    // After ClientPlugins are registered (inside ClientNetPlugin), wire
    // avian via lightyear so Position/Rotation sync follows the
    // replication path (Stage Q).
    wisp::add_avian_with_lightyear(&mut app);
    app.run();
}
