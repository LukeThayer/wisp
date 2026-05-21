use avian3d::prelude::*;
use bevy::{
    color::palettes::css,
    prelude::*,
    window::{CursorGrabMode, CursorOptions, PrimaryWindow},
};
use bevy_enhanced_input::prelude::ContextActivity;

use crate::input::{player_actions, radial_menu_actions, PlayerContext, RadialMenuContext};
use crate::magic::LensAnchor;
use crate::player::{spells::EquippedSpells, Facing, Player, PlayerCamera};
use crate::spatial::{GameLayer, PortalTraveler};

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
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
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

    // Test props — variety of cubes & spheres scattered in front of spawn.
    let mut prop_color = |seed: f32| {
        materials.add(StandardMaterial {
            base_color: Color::srgb(
                0.4 + 0.5 * (seed * 1.3).fract(),
                0.4 + 0.5 * (seed * 2.7).fract(),
                0.4 + 0.5 * (seed * 5.1).fract(),
            ),
            perceptual_roughness: 0.6,
            ..default()
        })
    };

    let cube_mesh = meshes.add(Cuboid::new(0.8, 0.8, 0.8));
    let sphere_mesh = meshes.add(Sphere::new(0.5));

    let props: [(Vec3, bool, f32, f32); 6] = [
        (Vec3::new(-2.5, 1.0, 0.0), true, 2.0, 0.1),
        (Vec3::new(0.0, 1.0, 0.0), false, 3.5, 0.4),
        (Vec3::new(2.5, 1.0, -0.5), true, 1.0, 0.7),
        (Vec3::new(-1.2, 1.0, -2.5), false, 1.5, 1.0),
        (Vec3::new(1.5, 1.0, -3.0), true, 4.0, 1.5),
        (Vec3::new(0.3, 1.0, -5.0), false, 2.5, 2.1),
    ];

    for (pos, is_cube, mass, seed) in props {
        let (mesh, collider) = if is_cube {
            (cube_mesh.clone(), Collider::cuboid(0.8, 0.8, 0.8))
        } else {
            (sphere_mesh.clone(), Collider::sphere(0.5))
        };
        commands.spawn((
            Name::new("Prop"),
            Mesh3d(mesh),
            MeshMaterial3d(prop_color(seed)),
            Transform::from_translation(pos),
            RigidBody::Dynamic,
            collider,
            Mass(mass),
            Friction::new(0.4),
            Restitution::new(0.1),
            CollisionLayers::new(GameLayer::Default, LayerMask::ALL),
            PortalTraveler,
        ));
    }

    // Player. The wand is a child of the camera (so it pitches with the view)
    // and carries the LensAnchor — its world transform is where the beam exits.
    let player_visuals = children![(
        PlayerCamera,
        Camera3d::default(),
        Projection::Perspective(PerspectiveProjection {
            fov: 90.0_f32.to_radians(),
            ..default()
        }),
        Transform::from_xyz(0.0, 0.7, 0.0),
        children![(
            LensAnchor,
            Mesh3d(meshes.add(Cuboid::new(0.05, 0.05, 0.4))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::from(css::GOLD),
                ..default()
            })),
            Transform::from_xyz(0.25, -0.3, -0.5),
        )],
    )];

    commands
        .spawn((
            Name::new("Player"),
            Player,
            Facing::default(),
            EquippedSpells::default(),
            Transform::from_xyz(0.0, 1.5, 5.0),
            Visibility::default(),
            (
                RigidBody::Dynamic,
                Collider::capsule(0.4, 1.2),
                LockedAxes::ROTATION_LOCKED,
                Mass(80.0),
                LinearDamping(0.5),
                Friction::new(0.0),
                Restitution::new(0.0),
                CollisionLayers::new(GameLayer::Player, LayerMask::ALL),
                // Origin just inside the capsule's bottom (capsule local extends
                // to y=-1.0); max_distance is the ground-clearance threshold.
                RayCaster::new(Vec3::new(0.0, -0.9, 0.0), Dir3::NEG_Y)
                    .with_max_distance(0.25)
                    .with_max_hits(1),
            ),
            PlayerContext,
            ContextActivity::<PlayerContext>::ACTIVE,
            player_actions(),
            player_visuals,
        ))
        // Attach RadialMenuContext to the same entity so it shares actions.
        .insert((
            RadialMenuContext,
            ContextActivity::<RadialMenuContext>::INACTIVE,
            radial_menu_actions(),
        ));
}
