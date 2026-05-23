//! Built-in effect implementations. Like deliveries, these take an exclusive
//! `&mut World` plus a `CastContext` and a `DeliveryTarget`.

use avian3d::prelude::*;
use bevy::ecs::system::SystemState;
use bevy::prelude::*;

use crate::spells::data::MarkerKind;
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
