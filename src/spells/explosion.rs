//! Server-only AoE explosion handler. Registered under
//! `HandlerId("explosion.impulse")`; used by `explosion_small.spell.ron`
//! and any future "blast on contact" spell.
//!
//! Reads the cast's effective origin (which for child casts is
//! `OriginDef::Impact(point)` — the collision point), the area shape from
//! `ctx.target`, and scales the impulse magnitude by `ctx.captured_charge`
//! (parent's stored charge). Spatial-queries every entity in range and
//! applies an outward linear impulse.
//!
//! Clients never register this handler — the catalog warns at load (not
//! an error) and runtime dispatch on a client just no-ops if it ever
//! happens. Child-cast dispatch only runs on the server today.

use avian3d::prelude::*;
use bevy::ecs::system::SystemState;
use bevy::prelude::*;
use serde_json::json;

use crate::spells::data::HandlerId;
use crate::spells::deliveries::{area_intersections, resolve_origin};
use crate::spells::handlers::{CastContext, HandlerRegistry};
use crate::trace;

/// HandlerId that `explosion_small.spell.ron`'s `Custom` payload references.
pub const EXPLOSION_HANDLER_ID: &str = "explosion.impulse";

/// Baseline impulse magnitude at `captured_charge == 1.0`. Tuned for the
/// fireball test: a stack of light rocks should be visibly tossed without
/// hurling them into the next region.
const BASE_IMPULSE: f32 = 200.0;

pub struct ExplosionPlugin;

impl Plugin for ExplosionPlugin {
    fn build(&self, app: &mut App) {
        let mut registry = app.world_mut().resource_mut::<HandlerRegistry>();
        registry.register(HandlerId(EXPLOSION_HANDLER_ID.to_string()), apply_explosion);
    }
}

fn apply_explosion(world: &mut World, ctx: &CastContext) {
    let Some((origin_pos, _)) = resolve_origin(world, ctx) else {
        warn!(
            "explosion.impulse: failed to resolve origin for {:?}",
            ctx.cast_id.0
        );
        return;
    };
    let scale = ctx.captured_charge.unwrap_or(1.0);
    let impulse_mag = BASE_IMPULSE * scale;

    let hits = area_intersections(world, ctx, origin_pos);
    let hit_count = hits.len();

    // Collect target positions first (read-only borrow), then apply
    // impulses (mutable borrow) — avoids fighting the borrow checker
    // when iterating the hit list.
    let target_positions: Vec<(Entity, Vec3)> = {
        let mut sys_state: SystemState<Query<&GlobalTransform>> = SystemState::new(world);
        let transforms = sys_state.get(world);
        hits.iter()
            .filter_map(|e| transforms.get(*e).ok().map(|tf| (*e, tf.translation())))
            .collect()
    };

    let mut applied = 0u32;
    {
        let mut sys_state: SystemState<Query<Forces>> = SystemState::new(world);
        let mut forces = sys_state.get_mut(world);
        for (entity, target_pos) in &target_positions {
            let direction = (*target_pos - origin_pos).normalize_or_zero();
            if direction == Vec3::ZERO {
                continue;
            }
            if let Ok(mut f) = forces.get_mut(*entity) {
                f.apply_linear_impulse(direction * impulse_mag);
                applied += 1;
            }
        }
    }

    trace::event(
        "explosion_applied",
        json!({
            "spell": ctx.spell_id.0,
            "cast":  ctx.cast_id.0,
            "origin": [origin_pos.x, origin_pos.y, origin_pos.z],
            "hits": hit_count,
            "applied": applied,
            "charge": scale,
            "chain_depth": ctx.chain_depth,
        }),
    );
}
