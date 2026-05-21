use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_inspector_egui::{bevy_egui::EguiPlugin, quick::WorldInspectorPlugin};

mod input;
mod magic;
mod player;
mod spatial;
mod ui;
mod world;

fn main() {
    App::new()
        .add_plugins((
            DefaultPlugins.set(WindowPlugin {
                primary_window: Some(Window {
                    title: "wisp".to_string(),
                    ..default()
                }),
                ..default()
            }),
            PhysicsPlugins::default(),
            EguiPlugin::default(),
            WorldInspectorPlugin::new(),
            input::InputPlugin,
            player::PlayerPlugin,
            magic::MagicPlugin,
            spatial::SpatialMagicPlugin,
            ui::UiPlugin,
            world::WorldPlugin,
        ))
        .run();
}
