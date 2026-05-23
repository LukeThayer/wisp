//! Built-in payload implementations. Each function takes an exclusive
//! `&mut World` plus a `CastContext`, materializes the payload, and returns
//! a `DeliveryTarget` so the effect step can act on the hit/spawned entity.
//!
//! Bespoke payloads (lens optics, portal placement, iris flash) live in
//! their spell modules and register under `PayloadDef::Custom`.

use avian3d::prelude::*;
use bevy::ecs::system::SystemState;
use bevy::prelude::*;
use lightyear::prelude::MessageSender;

use crate::net::protocol::{
    ParentCastInfo, PlayerInputChannel, PropShape, SpawnBodyMessage,
};
use crate::player::PlayerRig;
use crate::spells::bodies::{BodyDef, ColliderShape};
use crate::spells::catalog::BodyCatalog;
use crate::spells::data::{AreaShape, BodyTemplateId, LaunchKind, OriginDef, TargetDef};
use crate::spells::engine::DeliveryTarget;
use crate::spells::handlers::CastContext;

/// Generic server-authoritative SpawnBody delivery. Looks up the body
/// template in `BodyCatalog`, computes the launch velocity from the
/// caster's wand pose, then sends `SpawnBodyMessage` so the server
/// materializes a `NetworkedProp` and replicates it to every client. No
/// local entity is created on the caster — the replication round-trip
/// is how the caster sees their own projectile too.
///
/// Adding a new projectile-style spell is now RON-only: write a
/// `.body.ron` for the shape/mass/material and a `.spell.ron` with
/// `delivery: SpawnBody(template: "name", launch: Throw(...))`.
pub fn spawn_body(
    world: &mut World,
    ctx: &CastContext,
    template: BodyTemplateId,
    launch: LaunchKind,
) -> DeliveryTarget {
    let Some((origin_pos, origin_forward)) = resolve_origin(world, ctx) else {
        return DeliveryTarget::default();
    };

    let def = {
        let catalog = world.resource::<BodyCatalog>();
        let Some(def) = catalog.get(&template) else {
            warn!(
                "Cast {:?}: body template {:?} not in catalog",
                ctx.cast_id.0, template.0
            );
            return DeliveryTarget::default();
        };
        def.clone()
    };

    let velocity = match launch {
        LaunchKind::Throw { forward, up } => origin_forward * forward + Vec3::Y * up,
        LaunchKind::Drop | LaunchKind::Held => Vec3::ZERO,
    };

    // Offset forward from origin to keep the body from clipping through
    // the caster's capsule (for Wand origin) or through whatever the
    // collision was against (for Impact origin).
    let spawn_pos = origin_pos + origin_forward * 0.4;
    let parent = ParentCastInfo {
        spell_id: ctx.spell_id.0.clone(),
        cast_id: ctx.cast_id.0.clone(),
        captured_charge: ctx.captured_charge,
        chain_depth: ctx.chain_depth,
    };
    send_spawn_body_message(world, &def, spawn_pos, velocity, Some(parent));

    DeliveryTarget {
        entity: None,
        direction: Some(origin_forward),
    }
}

/// Raycasts from the origin (wand by default; Impact point for child casts)
/// along the resolved forward direction. Returns the first hit entity,
/// excluding the caster. Portal awareness is not implemented in the
/// built-in path — spells that need it use a custom delivery.
pub fn hitscan(world: &mut World, ctx: &CastContext, max_range: f32) -> DeliveryTarget {
    let Some((origin_pos, origin_forward)) = resolve_origin(world, ctx) else {
        return DeliveryTarget::default();
    };
    let Ok(dir) = Dir3::new(origin_forward) else {
        return DeliveryTarget::default();
    };
    let excluded = vec![ctx.player];
    let filter = SpatialQueryFilter::from_excluded_entities(excluded);

    let mut sys_state: SystemState<SpatialQuery> = SystemState::new(world);
    let spatial = sys_state.get_mut(world);
    let hit = spatial.cast_ray(origin_pos, dir, max_range, true, &filter);

    DeliveryTarget {
        entity: hit.map(|h| h.entity),
        direction: Some(origin_forward),
    }
}

/// Beam: identical to hitscan, but the caller treats the target as a
/// continuously-pushed target (the effect step runs every frame while the
/// cast is in Channeling phase). Built-in beam doesn't spawn a beam mesh —
/// visualization is the responsibility of bespoke handlers / future
/// `BeamVisual` registry. Stage C ships the gameplay; visuals come later.
pub fn beam(world: &mut World, ctx: &CastContext, max_range: f32) -> DeliveryTarget {
    hitscan(world, ctx, max_range)
}

/// Build a `SpawnBodyMessage` from a `BodyDef` + spawn pose and dispatch
/// it via the local client's `MessageSender`. If the body's collider shape
/// can't be mapped onto the wire-level `PropShape` (e.g. cylinder/capsule),
/// the spawn is skipped with a warning rather than silently sending a wrong
/// shape.
fn send_spawn_body_message(
    world: &mut World,
    def: &BodyDef,
    origin: Vec3,
    velocity: Vec3,
    parent_cast: Option<ParentCastInfo>,
) {
    let shape = match prop_shape_from_collider(&def.physics.collider) {
        Some(s) => s,
        None => {
            warn!(
                "Body {:?}: collider shape unsupported by network protocol — \
                 wire only carries cube/sphere; extend SpawnBodyMessage to add more.",
                def.id.0
            );
            return;
        }
    };
    let msg = SpawnBodyMessage {
        origin: [origin.x, origin.y, origin.z],
        velocity: [velocity.x, velocity.y, velocity.z],
        shape,
        mass: def.physics.mass,
        friction: def.physics.friction,
        linear_damping: def.physics.linear_damping,
        angular_damping: def.physics.angular_damping,
        restitution: def.physics.restitution,
        // No per-instance tint variation yet; clients render with the
        // template's material via the existing NetworkedProp observer.
        tint_seed: 0.0,
        parent_cast,
    };

    let mut sys_state: SystemState<Option<Single<&mut MessageSender<SpawnBodyMessage>>>> =
        SystemState::new(world);
    let sender = sys_state.get_mut(world);
    if let Some(mut sender) = sender {
        let _ = sender.send::<PlayerInputChannel>(msg);
    }
}

fn prop_shape_from_collider(c: &ColliderShape) -> Option<PropShape> {
    match c {
        ColliderShape::Sphere { radius } => Some(PropShape::Sphere { radius: *radius }),
        ColliderShape::Cuboid { x, y, z } if (x - y).abs() < 1e-3 && (y - z).abs() < 1e-3 => {
            Some(PropShape::Cube { size: *x })
        }
        // Non-cube cuboids are common enough that we treat them as a
        // bounding cube using the average side. Replace this with a
        // proper `PropShape::Cuboid { x, y, z }` if a spell needs it.
        ColliderShape::Cuboid { x, y, z } => Some(PropShape::Cube {
            size: (*x + *y + *z) / 3.0,
        }),
        ColliderShape::Cylinder { .. } | ColliderShape::Capsule { .. } => None,
    }
}

/// Resolve the cast's effective origin into a (world_position, forward)
/// pair. Built-in payloads call this rather than reading the wand pose
/// directly so they automatically honor `OriginDef::Impact` overrides
/// from child casts.
///
/// - `OriginDef::Wand`: reads `PlayerRig.lens_anchor`'s global transform.
///   Returns `None` if the caster entity doesn't have a rig (e.g. child
///   cast with an orphan caster).
/// - `OriginDef::Self_`: caster's transform origin and forward.
/// - `OriginDef::Impact(point)`: the explicit world point; forward is
///   `+Z` as a placeholder (handlers that consume direction for Impact
///   origin should be aware there's no natural facing at a collision
///   point).
pub fn resolve_origin(world: &mut World, ctx: &CastContext) -> Option<(Vec3, Vec3)> {
    match &ctx.origin {
        OriginDef::Wand => {
            let rig = world.get::<PlayerRig>(ctx.player)?;
            let lens_anchor = rig.lens_anchor;
            let tf = world.get::<GlobalTransform>(lens_anchor)?;
            Some((tf.translation(), *tf.compute_transform().forward()))
        }
        OriginDef::Self_ => {
            let tf = world.get::<GlobalTransform>(ctx.player)?;
            Some((tf.translation(), *tf.compute_transform().forward()))
        }
        OriginDef::Impact(point) => Some((*point, Vec3::Z)),
    }
}

/// Spatial query helper for area-effect payloads. Given a cast with
/// `TargetDef::Area { shape }`, returns every entity whose collider
/// intersects the shape positioned at `origin_pos`. Caller decides what
/// to do with the hits (apply impulse, damage, despawn, etc.). Filter
/// excludes `ctx.player` to keep the caster from self-AoEing.
///
/// Returns an empty vec if the cast's target isn't `Area`.
pub fn area_intersections(
    world: &mut World,
    ctx: &CastContext,
    origin_pos: Vec3,
) -> Vec<Entity> {
    let TargetDef::Area { shape } = &ctx.target else {
        return Vec::new();
    };
    let collider = match shape {
        AreaShape::Sphere { radius } => Collider::sphere(*radius),
    };
    let excluded = vec![ctx.player];
    let filter = SpatialQueryFilter::from_excluded_entities(excluded);

    let mut sys_state: SystemState<SpatialQuery> = SystemState::new(world);
    let spatial = sys_state.get_mut(world);
    spatial.shape_intersections(&collider, origin_pos, Quat::IDENTITY, &filter)
}
