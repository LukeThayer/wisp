//! Arena setup: ground, lights, test props, and the local player. The
//! player itself is built by `player::spawn_player`; the arena builds
//! everything else.

use avian3d::prelude::*;
use bevy::{
    prelude::*,
    window::{CursorGrabMode, CursorOptions, PrimaryWindow},
};

use crate::physics::GameLayer;
use crate::player::{spawn_player, PlayerSpawn};
use crate::spells::SpellRegistry;

pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, (spawn_world, lock_cursor));
    }
}

fn lock_cursor(mut cursor: Single<&mut CursorOptions, With<PrimaryWindow>>) {
    cursor.grab_mode = CursorGrabMode::Locked;
    cursor.visible = false;
}

fn spawn_world(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    registry: Res<SpellRegistry>,
) {
    spawn_arena(&mut commands, &mut meshes, &mut materials);

    spawn_player(
        &mut commands,
        &asset_server,
        &mut meshes,
        &mut materials,
        &registry,
        PlayerSpawn::default(),
    );
}

fn spawn_arena(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
) {
    // Ground
    let ground_size = Vec3::new(50.0, 0.5, 50.0);
    commands.spawn((
        Name::new("Ground"),
        Mesh3d(meshes.add(Cuboid::new(ground_size.x, ground_size.y, ground_size.z))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(0.35, 0.36, 0.4),
            perceptual_roughness: 0.9,
            ..default()
        })),
        Transform::from_xyz(0.0, -0.25, 0.0),
        RigidBody::Static,
        Collider::cuboid(ground_size.x, ground_size.y, ground_size.z),
        CollisionLayers::new(GameLayer::Ground, LayerMask::ALL),
    ));

    // Lights
    commands.spawn((
        Name::new("Sun"),
        DirectionalLight {
            illuminance: 12_000.0,
            shadows_enabled: true,
            ..default()
        },
        Transform::from_xyz(8.0, 16.0, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: Color::WHITE,
        brightness: 120.0,
        ..default()
    });

    // Props are now spawned authoritatively by the server in
    // `net::server::spawn_arena`. The replication observer in
    // `net::replication` adds the mesh + static collider on receive.
}
