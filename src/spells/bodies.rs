//! Body templates: data-defined physical objects that spells can spawn or
//! place. A `BodyDef` describes the mesh, material, physics, optional light,
//! and identity markers. The framework's SpawnBody / Place deliveries
//! materialize an entity from one of these.
//!
//! Authors write `.body.ron` files under `assets/bodies/`; the body catalog
//! resolves named references at cast time.

use avian3d::prelude::*;
use bevy::prelude::*;
use serde::Deserialize;

use crate::physics::GameLayer;
use crate::spells::data::{BodyTemplateId, MarkerKind};
use crate::spells::markers::{FrostSpike, Lantern, RollingGlacier};
use crate::spells::portal::PortalTraveler;

#[derive(Asset, TypePath, Deserialize, Clone, Debug)]
pub struct BodyDef {
    pub id: BodyTemplateId,
    pub mesh: MeshDef,
    pub material: MaterialDef,
    pub physics: PhysicsDef,
    #[serde(default)]
    pub light: Option<LightDef>,
    #[serde(default)]
    pub markers: Vec<MarkerKind>,
}

#[derive(Deserialize, Clone, Debug)]
pub enum MeshDef {
    Sphere { radius: f32 },
    Cuboid { x: f32, y: f32, z: f32 },
    Cylinder { radius: f32, height: f32 },
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct MaterialDef {
    pub base_color: ColorDef,
    #[serde(default)]
    pub emissive: ColorDef,
    #[serde(default = "default_roughness")]
    pub roughness: f32,
}

fn default_roughness() -> f32 {
    0.5
}

#[derive(Deserialize, Clone, Copy, Debug, Default)]
pub struct ColorDef {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    #[serde(default = "default_alpha")]
    pub a: f32,
}

fn default_alpha() -> f32 {
    1.0
}

impl ColorDef {
    pub fn to_color(self) -> Color {
        Color::srgba(self.r, self.g, self.b, self.a)
    }
    pub fn to_linear(self) -> LinearRgba {
        LinearRgba::new(self.r, self.g, self.b, self.a)
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct PhysicsDef {
    #[serde(default)]
    pub body: RigidBodyKind,
    pub collider: ColliderShape,
    #[serde(default = "default_mass")]
    pub mass: f32,
    #[serde(default)]
    pub friction: f32,
    #[serde(default)]
    pub linear_damping: f32,
    #[serde(default)]
    pub angular_damping: f32,
    #[serde(default)]
    pub restitution: f32,
}

fn default_mass() -> f32 {
    1.0
}

#[derive(Deserialize, Clone, Copy, Debug, Default)]
pub enum RigidBodyKind {
    #[default]
    Dynamic,
    Static,
    Kinematic,
}

#[derive(Deserialize, Clone, Debug)]
pub enum ColliderShape {
    Sphere { radius: f32 },
    Cuboid { x: f32, y: f32, z: f32 },
    Cylinder { radius: f32, height: f32 },
    Capsule { radius: f32, height: f32 },
}

#[derive(Deserialize, Clone, Debug)]
pub struct LightDef {
    pub color: ColorDef,
    pub intensity: f32,
    pub range: f32,
}

/// Materialize a body from a [`BodyDef`] at the given world transform with
/// the given initial linear velocity. Returns the new entity.
///
/// Markers in `def.markers` translate to concrete Bevy components
/// (`Lantern`, `PortalTraveler`, …). Adding a new marker kind requires
/// extending [`MarkerKind`] and this dispatch.
pub fn spawn_body(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    def: &BodyDef,
    transform: Transform,
    velocity: Vec3,
) -> Entity {
    let mesh_handle = meshes.add(build_mesh(&def.mesh));
    let material_handle = materials.add(StandardMaterial {
        base_color: def.material.base_color.to_color(),
        emissive: def.material.emissive.to_linear(),
        perceptual_roughness: def.material.roughness,
        ..default()
    });

    let collider = build_collider(&def.physics.collider);

    let rigid_body = match def.physics.body {
        RigidBodyKind::Dynamic => RigidBody::Dynamic,
        RigidBodyKind::Static => RigidBody::Static,
        RigidBodyKind::Kinematic => RigidBody::Kinematic,
    };

    let mut entity = commands.spawn((
        Name::new(def.id.0.clone()),
        Mesh3d(mesh_handle),
        MeshMaterial3d(material_handle),
        transform,
        (
            rigid_body,
            collider,
            Mass(def.physics.mass),
            LinearVelocity(velocity),
            Friction::new(def.physics.friction),
            LinearDamping(def.physics.linear_damping),
            AngularDamping(def.physics.angular_damping),
            Restitution::new(def.physics.restitution),
            CollisionLayers::new(GameLayer::Default, LayerMask::ALL),
        ),
    ));

    if let Some(light) = &def.light {
        entity.with_children(|c| {
            c.spawn((
                Name::new(format!("{}-Light", def.id.0)),
                PointLight {
                    color: light.color.to_color(),
                    intensity: light.intensity,
                    range: light.range,
                    shadows_enabled: false,
                    ..default()
                },
                Transform::default(),
            ));
        });
    }

    for marker in &def.markers {
        match marker {
            MarkerKind::Lantern => {
                entity.insert(Lantern);
            }
            MarkerKind::PortalTraveler => {
                entity.insert(PortalTraveler);
            }
            MarkerKind::RollingGlacier => {
                entity.insert(RollingGlacier::default());
            }
            MarkerKind::FrostSpike => {
                // Body-spawner path is currently unused — `frost_spire`
                // bypasses this and spawns its own configured spike in
                // `spells::ice::handle_frost_spire`. Placeholder zeros
                // are safe: 0 damage means `attach_spike_hitbox`'s
                // Hitbox is a no-op; 0 rise_remaining keeps the body
                // in whatever state the body def chose.
                entity.insert(FrostSpike {
                    lifetime: 180.0,
                    rise_remaining: 0.0,
                    damage: 0.0,
                    caster: None,
                });
            }
        }
    }

    entity.id()
}

fn build_mesh(mesh: &MeshDef) -> Mesh {
    match mesh {
        MeshDef::Sphere { radius } => Sphere::new(*radius).into(),
        MeshDef::Cuboid { x, y, z } => Cuboid::new(*x, *y, *z).into(),
        MeshDef::Cylinder { radius, height } => Cylinder::new(*radius, *height).into(),
    }
}

fn build_collider(shape: &ColliderShape) -> Collider {
    match shape {
        ColliderShape::Sphere { radius } => Collider::sphere(*radius),
        ColliderShape::Cuboid { x, y, z } => Collider::cuboid(*x, *y, *z),
        ColliderShape::Cylinder { radius, height } => Collider::cylinder(*radius, *height),
        ColliderShape::Capsule { radius, height } => Collider::capsule(*radius, *height),
    }
}
