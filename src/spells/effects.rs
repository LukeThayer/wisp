//! Built-in effect implementations. Like deliveries, these take an exclusive
//! `&mut World` plus a `CastContext` and a `DeliveryTarget`.

use avian3d::prelude::*;
use bevy::ecs::system::SystemState;
use bevy::prelude::*;

use crate::spells::damage::{apply_aoe_damage, FalloffKind, Team};
use crate::spells::data::{AreaShape, MarkerKind, TargetDef};
use crate::spells::deliveries::resolve_origin;
use crate::spells::engine::DeliveryTarget;
use crate::spells::handlers::CastContext;
use crate::spells::markers::Lantern;
use crate::spells::portal::PortalTraveler;

/// Apply a one-shot impulse to the delivery target. If the cast had a
/// `captured_charge` (PressReleaseCharged with a charge phase), the
/// magnitude is auto-scaled by that captured value.
pub fn impulse(
    world: &mut World,
    ctx: &CastContext,
    target: DeliveryTarget,
    magnitude: f32,
) {
    let Some(entity) = target.entity else { return; };
    let Some(direction) = target.direction else { return; };
    let scale = ctx.captured_charge.unwrap_or(1.0);
    apply_impulse(world, entity, direction * magnitude * scale);
}

/// Apply a per-second impulse (used while channeling). Reads `Time` for dt.
pub fn impulse_tick(
    world: &mut World,
    _ctx: &CastContext,
    target: DeliveryTarget,
    magnitude_per_sec: f32,
) {
    let Some(entity) = target.entity else { return; };
    let Some(direction) = target.direction else { return; };
    let dt = world.resource::<Time>().delta_secs();
    apply_impulse(world, entity, direction * magnitude_per_sec * dt);
}

/// Despawn whatever the delivery hit.
pub fn despawn(world: &mut World, target: DeliveryTarget) {
    if let Some(entity) = target.entity {
        if let Ok(entity_mut) = world.get_entity_mut(entity) {
            entity_mut.despawn();
        }
    }
}

/// Find every entity within `radius` of the caster that carries `marker`
/// and despawn it. Used for lantern pickup (caster radius = pickup range).
pub fn despawn_nearby_matching(
    world: &mut World,
    ctx: &CastContext,
    marker: MarkerKind,
    radius: f32,
) {
    let Some(caster_pos) = world.get::<Transform>(ctx.player).map(|t| t.translation) else {
        return;
    };

    let mut to_despawn: Vec<Entity> = Vec::new();
    match marker {
        MarkerKind::Lantern => {
            let mut q = world.query_filtered::<(Entity, &Transform), With<Lantern>>();
            for (e, tf) in q.iter(world) {
                if caster_pos.distance(tf.translation) <= radius {
                    to_despawn.push(e);
                }
            }
        }
        MarkerKind::PortalTraveler => {
            let mut q = world.query_filtered::<(Entity, &Transform), With<PortalTraveler>>();
            for (e, tf) in q.iter(world) {
                if caster_pos.distance(tf.translation) <= radius {
                    to_despawn.push(e);
                }
            }
        }
    }
    for e in to_despawn {
        if let Ok(em) = world.get_entity_mut(e) {
            em.despawn();
        }
    }
}

fn apply_impulse(world: &mut World, entity: Entity, impulse: Vec3) {
    let mut sys_state: SystemState<Query<Forces>> = SystemState::new(world);
    let mut forces = sys_state.get_mut(world);
    if let Ok(mut f) = forces.get_mut(entity) {
        f.apply_linear_impulse(impulse);
    }
}

/// Area damage centered on the cast's resolved origin, sized to the
/// cast's `TargetDef::Area::Sphere` radius. Scaled by `captured_charge`
/// when the cast carries one. Routes through `damage::apply_aoe_damage`
/// which handles the spatial query + per-target damage + DeathEvent.
///
/// `source_team` is `Neutral` so PvP works out of the box — a player's
/// fireball damages other Players (and Hostiles, and Neutrals). The
/// caster is excluded by entity (not by team) inside apply_aoe_damage,
/// so casters can't self-damage with their own spells. Switch this to
/// the caster's actual team when team-restricted AoE spells appear.
pub fn area_damage(
    world: &mut World,
    ctx: &CastContext,
    base_damage: f32,
    falloff: FalloffKind,
) {
    let TargetDef::Area { shape: AreaShape::Sphere { radius } } = &ctx.target else {
        warn!(
            "AreaDamage on cast {:?} requires TargetDef::Area::Sphere",
            ctx.cast_id.0
        );
        return;
    };
    let Some((origin_pos, _)) = resolve_origin(world, ctx) else {
        return;
    };
    let scale = ctx.captured_charge.unwrap_or(1.0);
    let damage = base_damage * scale;
    apply_aoe_damage(
        world,
        origin_pos,
        *radius,
        damage,
        falloff,
        // Caster is `ctx.player` — propagated by `dispatch_child_cast`
        // through `original_caster`. None means orphan (caster gone).
        if ctx.player == Entity::PLACEHOLDER { None } else { Some(ctx.player) },
        Team::Neutral,
    );
}
