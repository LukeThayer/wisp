//! Body-lifecycle triggers: wires `PayloadDef::SpawnBody.on_event` to
//! avian collision (and future timer) events on the server, then routes
//! into `dispatch_child_cast` so a parent spell can spawn a body that
//! triggers a child spell.
//!
//! Server-only — clients receive the result via the existing replication
//! and child-cast effects (impulses on `NetworkedProp`, etc.). The
//! component definition lives here too so other modules can read it; the
//! client never attaches it.
//!
//! Pipeline each frame:
//! 1. `attach_collision_events`: any newly-spawned entity with
//!    `BodyTriggers` gets `CollisionEventsEnabled` so avian emits the
//!    `CollisionStart` message for it.
//! 2. Avian (between schedules) writes `CollisionStart` messages for
//!    pairs involving event-enabled entities.
//! 3. `enqueue_pending_collision_triggers` (`Update`) drains those
//!    messages, marks the body as fired, and queues a
//!    `PendingChildCast` for each `OnCollision` hook on the body.
//! 4. `dispatch_pending_child_casts` (`Update`, exclusive world) drains
//!    the queue and calls `engine::dispatch_child_cast` for each.
//! 5. The triggered body is despawned on the next tick.

use avian3d::prelude::{CollisionEventsEnabled, CollisionStart};
use bevy::prelude::*;

use crate::spells::data::{BodyEventTrigger, CastId, OriginDef, SpellId, TriggerSpec};
use crate::spells::engine::{dispatch_child_cast, ChildCastContext};

pub struct BodyTriggersPlugin;

impl Plugin for BodyTriggersPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PendingChildCasts>();
        app.add_systems(
            Update,
            (
                attach_collision_events,
                enqueue_pending_collision_triggers,
                // Exclusive system: only run when there's actual work.
                // Bevy syncs the schedule when reaching an exclusive
                // system, so an unconditionally-scheduled empty
                // dispatcher would add a per-frame stall against
                // `sync_prop_positions` / `sync_player_positions` /
                // lightyear send — visible as replication jitter on
                // the client. The `run_if` lets Bevy skip scheduling
                // entirely on idle frames.
                dispatch_pending_child_casts.run_if(has_pending_child_casts),
            )
                .chain(),
        );
    }
}

/// Attached server-side to bodies spawned with a non-empty `on_event`
/// list. Carries the trigger spec + chain state propagated from the
/// originating cast. The component is **not** replicated — clients render
/// the body normally and observe the result of the eventual child cast
/// via the existing replication paths.
#[derive(Component, Clone, Debug)]
pub struct BodyTriggers {
    pub events: Vec<BodyEventTrigger>,
    /// Originating caster, for damage / kill attribution. `None` if the
    /// caster has disconnected by the time the body triggers.
    pub original_caster: Option<Entity>,
    /// Parent's `captured_charge`, propagated to the child cast as a
    /// scale factor.
    pub captured_charge: Option<f32>,
    /// Parent's chain depth — incremented before child dispatch so the
    /// `MAX_CHILD_CAST_CHAIN_DEPTH` cap works.
    pub chain_depth: u8,
    /// The originating spell + cast (`fireball.throw`, etc.). Propagated
    /// to the child cast so trace correlation knows what triggered it.
    /// `None` only if the body was spawned outside the cast pipeline
    /// (e.g. via a hand-rolled message), which shouldn't happen today.
    pub caused_by: Option<TriggerSpec>,
    /// Set once the body has fired any trigger so a single body can't
    /// double-dispatch (avian emits a `CollisionStart` per surface and
    /// we only want one explosion per fireball).
    pub fired: bool,
}

/// Queued child-cast dispatches. The collision observer can't call
/// `dispatch_child_cast` directly (needs `&mut World`); it pushes one of
/// these into the resource queue, and the exclusive system drains.
///
/// `Resource<Vec<…>>` instead of `Messages<…>` so we can `.run_if` the
/// exclusive system on non-empty — `Messages` doesn't expose `is_empty`.
#[derive(Resource, Default)]
struct PendingChildCasts(Vec<PendingChildCast>);

#[derive(Clone, Debug)]
struct PendingChildCast {
    spell_id: SpellId,
    cast_id: CastId,
    origin: Vec3,
    original_caster: Option<Entity>,
    captured_charge: Option<f32>,
    chain_depth: u8,
    caused_by: Option<TriggerSpec>,
}

fn has_pending_child_casts(pending: Res<PendingChildCasts>) -> bool {
    !pending.0.is_empty()
}

/// Bodies spawned this frame with `BodyTriggers` get `CollisionEventsEnabled`
/// so avian emits `CollisionStart` messages for them. Without this, avian
/// drops collision events for the body to save bandwidth.
fn attach_collision_events(
    mut commands: Commands,
    new_bodies: Query<Entity, (Added<BodyTriggers>, Without<CollisionEventsEnabled>)>,
) {
    for entity in &new_bodies {
        commands.entity(entity).insert(CollisionEventsEnabled);
    }
}

/// Drain `CollisionStart` messages. For each pair involving an entity
/// with `BodyTriggers`, walk its event list and push a `PendingChildCast`
/// for each `OnCollision` hook. Mark the body as fired to one-shot it.
fn enqueue_pending_collision_triggers(
    mut collisions: MessageReader<CollisionStart>,
    mut bodies: Query<(Entity, &GlobalTransform, &mut BodyTriggers)>,
    mut pending: ResMut<PendingChildCasts>,
    mut commands: Commands,
) {
    for ev in collisions.read() {
        // Either collider may carry BodyTriggers. Probe both.
        for candidate in [ev.collider1, ev.collider2] {
            let Ok((entity, transform, mut triggers)) = bodies.get_mut(candidate)
            else {
                continue;
            };
            if triggers.fired {
                continue;
            }
            triggers.fired = true;
            let origin = transform.translation();
            for event in &triggers.events {
                if let BodyEventTrigger::OnCollision {
                    triggers: TriggerSpec { spell_id, cast_id },
                } = event
                {
                    pending.0.push(PendingChildCast {
                        spell_id: spell_id.clone(),
                        cast_id: cast_id.clone(),
                        origin,
                        original_caster: triggers.original_caster,
                        captured_charge: triggers.captured_charge,
                        chain_depth: triggers.chain_depth.saturating_add(1),
                        caused_by: triggers.caused_by.clone(),
                    });
                }
            }
            // One-shot: despawn the triggering body. If a use case
            // wants the body to survive its trigger (e.g. a sticky
            // grenade with multiple events), add a "persistent" flag to
            // BodyTriggers.
            commands.entity(entity).despawn();
        }
    }
}

/// Drain queued child casts and dispatch each. Exclusive world so
/// `dispatch_child_cast` can call payload + effect handlers freely.
/// Gated by `has_pending_child_casts` so this only runs on frames with
/// actual work — skips the exclusive sync point otherwise.
fn dispatch_pending_child_casts(world: &mut World) {
    let pending = std::mem::take(&mut world.resource_mut::<PendingChildCasts>().0);
    for p in pending {
        dispatch_child_cast(
            world,
            p.spell_id,
            p.cast_id,
            ChildCastContext {
                origin: OriginDef::Impact(p.origin),
                original_caster: p.original_caster,
                captured_charge: p.captured_charge,
                chain_depth: p.chain_depth,
                caused_by: p.caused_by,
            },
        );
    }
}
