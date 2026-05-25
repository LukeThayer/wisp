//! Identity-tag components used by data-driven spells. Each variant of
//! [`crate::spells::data::MarkerKind`] maps to one of these components. The
//! body spawner inserts them, conditions/effects query by them.

use bevy::prelude::*;

/// Placed lantern body. Acts as a beam source for the convex-lens family
/// of spells and as a `PortalTraveler` so it can fall through portals.
#[derive(Component)]
pub struct Lantern;

/// Rolling glacier ball — selects the server-side ice-magic systems
/// (trail drop, momentum-scaled hitbox, expiry AoE) in `spells::ice`.
#[derive(Component, Debug, Clone)]
pub struct RollingGlacier {
    /// Distance accumulated since the last frozen-ground tile drop.
    pub since_last_tile: f32,
    /// Last observed world position; used to integrate distance.
    pub last_pos: Option<Vec3>,
    /// Player entity that cast the glacier. Threaded onto the ball's
    /// `Hitbox.source` so the contact-damage system skips the caster
    /// (without this the ball hits the caster on frame 1 — the spawn
    /// origin is the wand, which overlaps the casting capsule).
    pub caster: Option<Entity>,
    /// Size multiplier driven by rolling over other glaciers' frost.
    /// Scales the radius of subsequent tile drops and the broadcast
    /// `Transform.scale` for the client-side visual. Starts at 1.0
    /// (matching the `glacier_ball.body.ron` collider) and grows when
    /// the ball overlaps frozen tiles laid down by other glaciers.
    pub size_mult: f32,
}

impl Default for RollingGlacier {
    fn default() -> Self {
        Self {
            since_last_tile: 0.0,
            last_pos: None,
            caster: None,
            size_mult: 1.0,
        }
    }
}

/// Ice spike body spawned by `frost_spire`. Server-side lifetime,
/// rise-from-ground animation, and one-shot damage window all live in
/// `spells::ice`. The spike starts Kinematic with an upward velocity so
/// it physically punts whatever it rises through (rolling glaciers,
/// players caught in the emergence), then converts to Static once the
/// rise phase ends. Damage only applies during the rise window.
#[derive(Component)]
pub struct FrostSpike {
    /// Total remaining lifetime in seconds. Ticked by
    /// `tick_frost_spike_lifetime`; the spike despawns at zero.
    pub lifetime: f32,
    /// Remaining rise duration. While > 0 the body is Kinematic with
    /// upward velocity and carries an active Hitbox. When it hits zero,
    /// `settle_frost_spike` zeros the Hitbox damage so the settled
    /// spike becomes a passive obstacle.
    pub rise_remaining: f32,
    /// Damage dealt to anything contacted during the rise window.
    pub damage: f32,
    /// Player entity that cast the spire. Threaded onto the spike's
    /// `Hitbox.source` so the contact-damage system's by-entity
    /// self-hit filter and same-team filter both treat the caster
    /// (and their teammates, when teams diverge) as immune.
    pub caster: Option<Entity>,
}
