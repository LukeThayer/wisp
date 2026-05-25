//! Ice magic: rolling glacier + frost spire. Server-authoritative for
//! anything that touches gameplay state (frozen ground tiles, hitbox
//! attachment, spike spawn, expiry AoE). The client only renders the
//! replicated tiles + cubes/spheres that the server stamps out.
//!
//! - `RollingGlacier` marker (server) → tile-trail drop + per-impact
//!   damage scaling by momentum.
//! - `FrostSpike` marker (server) → bespoke lifetime + Hitbox attach.
//! - `NetworkedFrozenGround` (replicated) → flat blue disc + low-friction
//!   static collider. Despawns after a short lifetime.
//! - `frost_spire.cast` (client handler) sends `FrostSpireMessage`; the
//!   server resolves the player's location, consumes nearby frozen ground,
//!   and spawns the spike.
//!
//! Replication note: NetworkedFrozenGround uses lightyear's component
//! replication for the marker only. The visual disc + collider live
//! locally on each peer.

use avian3d::prelude::*;
use bevy::prelude::*;
use lightyear::prelude::server::ClientOf;
use lightyear::prelude::{LocalId, MessageReceiver, MessageSender, PeerId, RemoteId};

use crate::net::protocol::{
    FrostSpireMessage, GlacierScaleBroadcast, NetworkedFrostSpike, NetworkedFrozenGround,
    NetworkedId, NetworkedPlayer, NetworkedPosition, NetworkedProp, PlayerInputChannel, PropShape,
};
use crate::physics::GameLayer;
use crate::player::{LocalPlayer, Player, PlayerRig};
use crate::spells::damage::{Hitbox, Team};
use crate::spells::data::HandlerId;
use crate::spells::handlers::{CastContext, HandlerRegistry};
use crate::spells::markers::{FrostSpike, RollingGlacier};

// --- Tunables -------------------------------------------------------------

/// How many meters the glacier rolls before dropping the next frozen
/// ground tile.
const GLACIER_TRAIL_STEP: f32 = 0.8;
/// XZ distance below which a new tile is considered "stacked" on an
/// existing one and gets skipped. Tiles are allowed to overlap freely
/// (curving trails / multi-glacier crossings); this only suppresses
/// the case where a second tile would land at essentially the same
/// spot as a previous one.
const GLACIER_TILE_DEDUP_DIST: f32 = 0.25;
/// Radius of each rolling-glacier trail tile.
const GLACIER_TRAIL_RADIUS: f32 = 0.45;
/// Authored radius of the glacier ball's collider — must match
/// `glacier_ball.body.ron`'s `mesh: Sphere(radius:)`. The growth
/// system multiplies this by `size_mult` to compute the new collider.
const GLACIER_BALL_BASE_RADIUS: f32 = 0.32;
/// Radius of the wider tile dropped when the glacier expires.
#[allow(dead_code)]
const GLACIER_EXPIRE_TILE_RADIUS: f32 = 3.0;
/// Frozen ground lifetime in seconds (3 minutes — frost stays around
/// long enough to build up arenas of ice across multiple casts).
const FROZEN_GROUND_LIFETIME: f32 = 180.0;
/// How long the ice spike sticks around once summoned (3 minutes — it
/// becomes a persistent piece of terrain, not a transient hit).
const FROST_SPIKE_LIFETIME: f32 = 180.0;
/// How long the spike is in its emergent / rising phase. During this
/// window the body is Kinematic with upward velocity, the Hitbox is
/// active, and contacts deal damage. After this elapses the spike
/// switches to Static and the Hitbox is removed. Kept short so the
/// emergence reads as a punch rather than a slow elevator — a default
/// spike covers ~2.6m in 0.15s ≈ 17 m/s, more than enough to launch
/// anything it rises through.
const FROST_SPIKE_RISE_DURATION: f32 = 0.15;
/// How deep the resting spike's base sits below the ground surface.
/// Slightly buried so the cuboid reads as growing *out of* the
/// surface instead of perched on it.
const FROST_SPIKE_GROUND_INSET: f32 = 0.08;
/// Base damage at the default tile size (`GLACIER_TRAIL_RADIUS`). The
/// final per-cast damage scales linearly with the consumed tile's
/// radius, then adds a small bonus per unit of captured charge.
const FROST_SPIKE_BASE_DAMAGE: f32 = 22.0;
/// Extra damage per unit of captured charge (0..1).
const FROST_SPIKE_DAMAGE_PER_CHARGE: f32 = 18.0;
/// Default spike footprint (slightly wider than the original
/// `ice_spike.body.ron` cuboid x/z so the smaller pillar still reads
/// as a chunky shard rather than a thin needle).
const FROST_SPIKE_BASE_WIDTH: f32 = 0.55;
/// Default spike height. Reduced from the original 2.6 — at the new
/// shape the spike feels like a tooth jutting out of the ground
/// rather than a wall.
const FROST_SPIKE_BASE_HEIGHT: f32 = 1.6;
/// Mass of the default-size spike. Scales cubically with size so a
/// grown spike actually has the inertia to shove a heavy glacier ball
/// without getting pushed back. Glacier mass is 6.0; a default spike
/// at 20kg already overwhelms it, and a 2× spike at 160kg sends one
/// flying.
const FROST_SPIKE_BASE_MASS: f32 = 20.0;
/// Range of the targeting raycast. 4× the portal placement range so
/// the player can spike a long ice trail laid down across an arena
/// from across the room.
const FROST_SPIRE_RANGE: f32 = 60.0;
/// Slack added to the server's per-source match tolerance — absorbs
/// one frame of motion or lag between the client raycast and the
/// server tick that handles it. The base tolerance is the source's
/// own bounding extent (tile radius for frozen ground, half-diagonal
/// for spikes) so a hit on the *top* of a tall spike still validates.
const FROST_SPIRE_MATCH_SLACK: f32 = 0.3;
/// Damage the glacier deals on each player/hurtbox contact. The
/// hitbox is *not* single-hit — the ball keeps rolling after impact
/// so it can plow through groups. `CollisionStart` semantics mean a
/// player only takes one hit per contact, not a per-frame tick while
/// being rolled over.
const GLACIER_CONTACT_DAMAGE: f32 = 28.0;
/// Snowball packing factor: per-contact mult bump from rolling over a
/// foreign tile is `tile.radius * factor / current_size_mult^3` —
/// cubic diminishing return. Doubling the ball's size makes the next
/// tile contribute 1/8 as much; quadrupling makes it 1/64. A tiny
/// glacier still gulps the first few foreign tiles, but reaching
/// stadium-size takes a *lot* of frost.
const GLACIER_GROWTH_FACTOR: f32 = 1.8;
/// Hard cap on `RollingGlacier.size_mult`. The growth formula
/// asymptotes naturally; this just guards against numerical drift /
/// pathological RON tuning.
const GLACIER_MAX_SIZE_MULT: f32 = 12.0;

/// Server-side lifetime tag for `NetworkedFrozenGround` tiles, plus
/// the glacier that laid them down. `creator` gates the growth rule:
/// a ball that rolls onto its own trail does *not* grow, only onto
/// frost it didn't make.
#[derive(Component, Default)]
pub struct FrozenGroundLife {
    pub age: f32,
    pub creator: Option<Entity>,
}

pub struct IcePlugin;

impl Plugin for IcePlugin {
    fn build(&self, app: &mut App) {
        // The cast engine is client-side; the bespoke server systems
        // here run unconditionally and just no-op if no `RollingGlacier`
        // / `FrostSpike` / `NetworkedFrozenGround` is in the world.
        app.add_systems(
            Update,
            (
                attach_glacier_hitbox,
                attach_spike_hitbox,
                grow_glacier_on_foreign_ice,
                drop_glacier_trail,
                tick_frozen_ground_lifetime,
                settle_frost_spike,
                tick_frost_spike_lifetime,
                handle_frost_spire,
            ),
        );
    }
}

/// Register cast handlers in the shared `HandlerRegistry`. Called from
/// `SpellsPlugin::build`, which is part of `build_shared` — wired by
/// the **client** bin only today. The server bin does not load
/// `SpellsPlugin`; it just inserts an empty `HandlerRegistry` so its
/// catalog validator doesn't warn. Server-side gameplay state for ice
/// magic lives in `IcePlugin`, added explicitly by the server bin.
///
/// The HUD indicator for frost-spire targeting is wired here too — it
/// only ever runs on the client, since `IceClientHudPlugin`'s systems
/// query `LocalPlayer` / `HudCenter` (both client-only).
pub fn register(app: &mut App) {
    {
        let mut registry = app.world_mut().resource_mut::<HandlerRegistry>();
        registry.register(
            HandlerId("frost_spire.cast".to_string()),
            frost_spire_handler,
        );
        registry.register(
            HandlerId("rolling_glacier.expire".to_string()),
            rolling_glacier_expire_handler,
        );
    }
    app.add_plugins(IceClientHudPlugin);
}

// --- Client-side cast handlers --------------------------------------------

/// Client handler for `frost_spire.cast`. Raycasts from the camera
/// forward (same pattern as the ground portal) and only ships a
/// `FrostSpireMessage` if the closest hit is an "ice surface" —
/// either a `NetworkedFrozenGround` tile (the canonical fuel) or an
/// existing `FrostSpike` cuboid (chain a spike off the side of
/// another). The cast still burns its cooldown when the raycast
/// misses; the gesture of "look at ice, click" is the affordance.
///
/// The hit normal is forwarded to the server so the spawned spike
/// orients along the surface — a wall-side hit on an existing spike
/// makes a horizontal spike branch off, a floor hit makes the usual
/// vertical pillar.
///
/// Server-side validation in `handle_frost_spire` re-confirms that an
/// ice source exists near the claimed hit point, so a malicious
/// client crafting a `FrostSpireMessage` for empty ground is ignored.
fn frost_spire_handler(world: &mut World, ctx: &CastContext) {
    use bevy::ecs::system::SystemState;
    let charge = ctx.captured_charge.unwrap_or(0.0);

    let mut sys_state: SystemState<(
        SpatialQuery,
        Single<&GlobalTransform, (With<crate::player::PlayerCamera>, With<LocalPlayer>)>,
        Single<Entity, (With<Player>, With<LocalPlayer>)>,
        Query<Entity, With<NetworkedFrozenGround>>,
        Query<Entity, With<NetworkedFrostSpike>>,
        Option<Single<&mut MessageSender<FrostSpireMessage>, Without<ClientOf>>>,
    )> = SystemState::new(world);
    let (spatial, cam, player_entity, frozen_tiles, spikes, sender) = sys_state.get_mut(world);
    let Some(mut sender) = sender else { return };

    let cam_pos = cam.translation();
    let forward = cam.rotation() * Vec3::NEG_Z;
    let Ok(dir) = Dir3::new(forward) else { return };

    let filter =
        SpatialQueryFilter::from_excluded_entities(std::iter::once(*player_entity).collect::<Vec<_>>());
    let Some(hit) = spatial.cast_ray(cam_pos, dir, FROST_SPIRE_RANGE, true, &filter) else {
        return;
    };
    let valid = frozen_tiles.get(hit.entity).is_ok() || spikes.get(hit.entity).is_ok();
    if !valid {
        return;
    }
    let hit_pos = cam_pos + forward * hit.distance;
    let n = hit.normal.normalize_or_zero();
    let _ = sender.send::<PlayerInputChannel>(FrostSpireMessage {
        origin: [hit_pos.x, hit_pos.y, hit_pos.z],
        normal: [n.x, n.y, n.z],
        captured_charge: charge,
    });
}

/// Client handler for the expire child cast — currently a no-op shim.
/// The damage half is handled by `EffectDef::AreaDamage` in the spell
/// RON; the visual frozen-ground patch is dropped by the server via
/// `drop_glacier_trail` while the ball is alive, so the expire only
/// needs to *exist* so the BodyTriggers `OnTimeout` resolves cleanly.
fn rolling_glacier_expire_handler(_world: &mut World, _ctx: &CastContext) {}

// --- Server systems --------------------------------------------------------

/// Stamp a `Hitbox` onto newly-spawned `RollingGlacier` bodies the first
/// time we see them. `source: glacier.caster` gates the contact-damage
/// system's self-hit filter — without it the ball would spawn at the
/// caster's wand (inside their capsule) and the very first physics
/// step would resolve the overlap, fire the hitbox, and destroy the
/// ball before it ever moves.
fn attach_glacier_hitbox(
    mut commands: Commands,
    new: Query<(Entity, &RollingGlacier), Without<Hitbox>>,
) {
    for (e, glacier) in &new {
        commands.entity(e).insert((
            Hitbox {
                damage: GLACIER_CONTACT_DAMAGE,
                team: Team::Neutral,
                source: glacier.caster,
                single_hit: false,
            },
            // Needed so avian emits `CollisionStart` events for the
            // ball — the growth system reads those to spot when the
            // ball overlaps another glacier's frozen ground.
            CollisionEventsEnabled,
        ));
    }
}

/// Attach the damage Hitbox the first time we see a new `FrostSpike`.
/// `single_hit: false` so the carrier (the spike) is *not* destroyed
/// on impact — the spike has to persist for 3 minutes as terrain. The
/// per-tick-multi-hit problem that flag normally guards against does
/// not apply here because `apply_contact_damage` runs off
/// `CollisionStart` (single event per contact-begin transition), and
/// the rise window is brief — `settle_frost_spike` zeroes this
/// Hitbox's damage the moment the spike comes to rest, so post-rise
/// contacts (player walks into a settled spike) cleanly deal no damage.
///
/// `team: Team::Players` plus `source: spike.caster` together implement
/// "only damages non-caster team": the same-team filter in
/// `Team::can_damage` skips all `Team::Players` hurtboxes (so allies
/// are immune), and the per-entity self-hit filter is belt-and-braces
/// against the caster damaging themselves should team rules diverge.
///
/// `CollisionEventsEnabled` is required so avian emits
/// `CollisionStart` for spike↔player pairs — without it the contact
/// fires but the event the damage system reads never lands.
fn attach_spike_hitbox(
    mut commands: Commands,
    new: Query<(Entity, &FrostSpike), Without<Hitbox>>,
) {
    for (e, spike) in &new {
        commands.entity(e).insert((
            Hitbox {
                damage: spike.damage,
                team: Team::Players,
                source: spike.caster,
                single_hit: false,
            },
            CollisionEventsEnabled,
        ));
    }
}

/// Watch `CollisionStart` events for ball ↔ frozen-tile pairs. If the
/// tile was created by a *different* glacier (or has no creator yet),
/// bump the ball's `size_mult` (capped at `GLACIER_MAX_SIZE_MULT`),
/// swap its collider for one matching the new size so it physically
/// occupies the right space, and broadcast the new scale to every
/// client so the visual grows too.
fn grow_glacier_on_foreign_ice(
    mut commands: Commands,
    mut collisions: MessageReader<CollisionStart>,
    mut glaciers: Query<(Entity, &NetworkedId, &mut RollingGlacier)>,
    tiles: Query<(&FrozenGroundLife, &NetworkedFrozenGround)>,
    mut broadcast: Query<&mut MessageSender<GlacierScaleBroadcast>, With<ClientOf>>,
) {
    for ev in collisions.read() {
        let (ball_entity, tile_entity) = match (
            glaciers.get(ev.collider1).is_ok(),
            tiles.get(ev.collider1).is_ok(),
            glaciers.get(ev.collider2).is_ok(),
            tiles.get(ev.collider2).is_ok(),
        ) {
            (true, _, _, true) => (ev.collider1, ev.collider2),
            (_, true, true, _) => (ev.collider2, ev.collider1),
            _ => continue,
        };
        let Ok((tile_life, tile_marker)) = tiles.get(tile_entity) else {
            continue;
        };
        if tile_life.creator == Some(ball_entity) {
            continue;
        }
        let Ok((_e, netid, mut glacier)) = glaciers.get_mut(ball_entity) else {
            continue;
        };
        if glacier.size_mult >= GLACIER_MAX_SIZE_MULT {
            continue;
        }

        // Pack-the-snowball formula with **cubic** diminishing return:
        // growth ∝ tile_radius / size_mult³. Going from 1× to 2×
        // takes a handful of small tiles; going from 4× to 8× takes
        // dozens.
        let denom = glacier.size_mult.max(0.1).powi(3);
        let growth = tile_marker.radius * GLACIER_GROWTH_FACTOR / denom;
        glacier.size_mult = (glacier.size_mult + growth).min(GLACIER_MAX_SIZE_MULT);
        let scale = glacier.size_mult;
        let net_id = netid.0;

        // Consume the tile — the frost packed onto the ball can't also
        // sit on the ground. Despawn cleans up the collider, mesh on
        // every client (via replication remove), and life-tick state.
        if let Ok(mut em) = commands.get_entity(tile_entity) {
            em.despawn();
        }

        // Server-side authoritative collider grows with the visual.
        commands
            .entity(ball_entity)
            .insert(Collider::sphere(GLACIER_BALL_BASE_RADIUS * scale));
        for mut sender in &mut broadcast {
            let _ = sender.send::<PlayerInputChannel>(GlacierScaleBroadcast { net_id, scale });
        }
    }
}

/// Drop a `NetworkedFrozenGround` tile every `GLACIER_TRAIL_STEP` meters
/// the ball travels. Raycasts straight down from the ball to find the
/// surface — tiles are placed at the **ground**, not at the ball's
/// current Y. Without this, mid-flight tiles would spawn in the air and
/// the ball bounces off its own trail.
fn drop_glacier_trail(
    mut commands: Commands,
    spatial: SpatialQuery,
    senders: Query<Entity, (With<ClientOf>, With<lightyear::prelude::Connected>)>,
    mut glaciers: Query<(Entity, &Transform, &mut RollingGlacier)>,
    existing_tiles: Query<(Entity, &Transform, &NetworkedFrozenGround), Without<RollingGlacier>>,
    mut id_alloc: ResMut<crate::net::server::NetworkedIdAlloc>,
) {
    let current_senders: Vec<Entity> = senders.iter().collect();
    if current_senders.is_empty() {
        return;
    }
    for (glacier_entity, tf, mut glacier) in &mut glaciers {
        let pos = tf.translation;
        let prev = glacier.last_pos.unwrap_or(pos);
        let step = pos.distance(prev);
        glacier.last_pos = Some(pos);
        glacier.since_last_tile += step;
        if glacier.since_last_tile < GLACIER_TRAIL_STEP {
            continue;
        }
        // Exclude the ball itself AND every existing frozen-ground tile
        // from the downcast — otherwise the ray hits the top of a tile
        // the ball is rolling across and places the next tile floating
        // 6 cm above the floor instead of on it.
        let mut excluded: Vec<Entity> = Vec::with_capacity(existing_tiles.iter().len() + 1);
        excluded.push(glacier_entity);
        excluded.extend(existing_tiles.iter().map(|(e, _, _)| e));
        let filter = SpatialQueryFilter::from_excluded_entities(excluded);
        let Some(hit) = spatial.cast_ray(pos, Dir3::NEG_Y, 4.0, true, &filter) else {
            glacier.since_last_tile = 0.0;
            continue;
        };
        glacier.since_last_tile = 0.0;
        let ground_pos = pos + Vec3::NEG_Y * hit.distance;
        let radius = GLACIER_TRAIL_RADIUS * glacier.size_mult;
        // Allow overlap (curving trails, growing balls covering each
        // other's frost), just prevent *stacking* — two tiles whose
        // centers are within a small radius of each other are visually
        // indistinguishable from each other. Threshold is a small
        // fraction of the new tile's radius so growth doesn't make
        // the de-dup zone shrink to nothing for big tiles.
        let new_xz = Vec2::new(ground_pos.x, ground_pos.z);
        let dedup_dist2 = (GLACIER_TILE_DEDUP_DIST).powi(2);
        let stacks_on_existing = existing_tiles.iter().any(|(_e, tile_tf, _tile)| {
            let dx = tile_tf.translation.x - new_xz.x;
            let dz = tile_tf.translation.z - new_xz.y;
            (dx * dx + dz * dz) < dedup_dist2
        });
        if stacks_on_existing {
            continue;
        }
        crate::trace::event(
            "glacier_tile_drop",
            serde_json::json!({
                "ball_pos": [pos.x, pos.y, pos.z],
                "tile_pos": [ground_pos.x, ground_pos.y, ground_pos.z],
                "ray_dist": hit.distance,
                "size_mult": glacier.size_mult,
                "radius": radius,
            }),
        );
        spawn_frozen_ground(
            &mut commands,
            &mut id_alloc,
            ground_pos,
            radius,
            Some(glacier_entity),
            &current_senders,
        );
    }
}

fn tick_frozen_ground_lifetime(
    time: Res<Time>,
    mut commands: Commands,
    mut tiles: Query<(Entity, &mut FrozenGroundLife)>,
) {
    let dt = time.delta_secs();
    for (e, mut life) in &mut tiles {
        life.age += dt;
        if life.age >= FROZEN_GROUND_LIFETIME {
            if let Ok(mut em) = commands.get_entity(e) {
                em.despawn();
            }
        }
    }
}

fn tick_frost_spike_lifetime(
    time: Res<Time>,
    mut commands: Commands,
    mut spikes: Query<(Entity, &mut FrostSpike)>,
) {
    let dt = time.delta_secs();
    for (e, mut spike) in &mut spikes {
        spike.lifetime -= dt;
        if spike.lifetime <= 0.0 {
            if let Ok(mut em) = commands.get_entity(e) {
                em.despawn();
            }
        }
    }
}

/// Tick the rise window. While `rise_remaining > 0` the spike is
/// Kinematic and carries an active Hitbox (set by `attach_spike_hitbox`).
/// When the rise ends we:
///   1. Zero the velocity so the spike doesn't drift forever.
///   2. Swap the body to `RigidBody::Static` — the spike becomes a
///      permanent obstacle that can be walked into, leaned on, etc.
///   3. Zero the Hitbox's damage so post-rise contacts deal nothing.
///      We *don't* remove the Hitbox component: `apply_contact_damage`
///      and this system are both in `Update` with no explicit order,
///      and removing on the transition frame would let one frame of
///      damage leak through if `apply_contact_damage` ran first. The
///      damage-amount path early-exits on `amount <= 0.0`, so zeroing
///      is race-free: the moment this system runs on the settle frame,
///      every subsequent damage check on this hitbox returns 0.
fn settle_frost_spike(
    time: Res<Time>,
    mut commands: Commands,
    mut spikes: Query<(
        Entity,
        &mut FrostSpike,
        Option<&mut LinearVelocity>,
        Option<&mut Hitbox>,
    )>,
) {
    let dt = time.delta_secs();
    for (entity, mut spike, vel, hitbox) in &mut spikes {
        if spike.rise_remaining <= 0.0 {
            continue;
        }
        spike.rise_remaining -= dt;
        if spike.rise_remaining > 0.0 {
            continue;
        }
        if let Some(mut v) = vel {
            v.0 = Vec3::ZERO;
        }
        if let Some(mut hb) = hitbox {
            hb.damage = 0.0;
        }
        if let Ok(mut em) = commands.get_entity(entity) {
            em.insert(RigidBody::Static);
        }
    }
}

/// Server handler for `FrostSpireMessage`.
///
/// Wire semantics: `origin` is the client's raycast hit point on an
/// ice surface (a `NetworkedFrozenGround` tile, or the side of an
/// existing `FrostSpike`), and `normal` is the surface normal — the
/// axis the new spike will emerge along.
///
/// Server re-validates by searching for any ice source within
/// `FROST_SPIRE_TILE_MATCH_RADIUS` of the claimed origin:
/// - Frozen ground tile → consume the tile, scale spike from
///   `tile.radius` (one tile in, one spike out).
/// - Existing spike → don't consume (chaining off another spike is
///   free), scale from the source spike's width.
///
/// The spawned spike is Kinematic with `LinearVelocity = normal *
/// rise_speed` and rotated so its local +Y axis points along
/// `normal`, so a wall-side cast on an existing spike produces a
/// horizontal branch and a floor cast still produces the canonical
/// vertical pillar. `settle_frost_spike` handles the rise-to-rest
/// transition; `tick_frost_spike_lifetime` despawns at 3 minutes.
fn handle_frost_spire(
    mut receivers: Query<(&RemoteId, &mut MessageReceiver<FrostSpireMessage>), With<ClientOf>>,
    senders: Query<Entity, (With<ClientOf>, With<lightyear::prelude::Connected>)>,
    mut commands: Commands,
    tiles: Query<(Entity, &Transform, &NetworkedFrozenGround)>,
    existing_spikes: Query<(Entity, &Transform, &NetworkedProp), With<NetworkedFrostSpike>>,
    mut id_alloc: ResMut<crate::net::server::NetworkedIdAlloc>,
    client_map: Res<crate::net::server::ClientPlayerMap>,
) {
    let current_senders: Vec<Entity> = senders.iter().collect();
    if current_senders.is_empty() {
        return;
    }
    for (RemoteId(peer_id), mut receiver) in &mut receivers {
        let client_id = match peer_id {
            PeerId::Netcode(id) | PeerId::Steam(id) | PeerId::Local(id) | PeerId::Entity(id) => {
                Some(*id)
            }
            _ => None,
        };
        let caster: Option<Entity> = client_id.and_then(|id| client_map.0.get(&id).copied());
        for msg in receiver.receive() {
            let origin = Vec3::new(msg.origin[0], msg.origin[1], msg.origin[2]);
            if !origin.is_finite() {
                continue;
            }
            // Sanitize the wire normal. Clients send the raycast hit
            // normal, but if it's garbage (NaN, zero-length) fall back
            // to +Y so we always have a usable emergence axis.
            let normal = Vec3::new(msg.normal[0], msg.normal[1], msg.normal[2])
                .try_normalize()
                .unwrap_or(Vec3::Y);

            // Find the closest ice source within its own size-aware
            // tolerance. Both kinds (tile, spike) are eligible. A hit
            // on the top of a tall spike or the rim of a big tile
            // would otherwise fall outside a fixed tolerance and
            // silently drop the message — per-source slack keeps the
            // validation tight without rejecting legitimate casts.
            let mut best_tile: Option<(Entity, f32, f32)> = None;
            for (e, tf, tile) in &tiles {
                let d2 = (tf.translation - origin).length_squared();
                let tol = tile.radius + FROST_SPIRE_MATCH_SLACK;
                if d2 > tol * tol {
                    continue;
                }
                if best_tile.map_or(true, |(_, prev_d2, _)| d2 < prev_d2) {
                    best_tile = Some((e, d2, tile.radius));
                }
            }
            let mut best_spike: Option<(f32, f32)> = None;
            for (_, tf, prop) in &existing_spikes {
                let d2 = (tf.translation - origin).length_squared();
                // Per-spike tolerance = the spike's longest half-extent
                // plus slack, so a raycast hit anywhere on the cuboid
                // surface (top, side, corner) validates regardless of
                // how scaled-up the source spike is.
                let (src_w, src_h) = match prop.shape {
                    PropShape::Cuboid { x, y, .. } => (x, y),
                    _ => (FROST_SPIKE_BASE_WIDTH, FROST_SPIKE_BASE_HEIGHT),
                };
                let half_max = src_w.max(src_h) * 0.5;
                let tol = half_max + FROST_SPIRE_MATCH_SLACK;
                if d2 > tol * tol {
                    continue;
                }
                if best_spike.map_or(true, |(prev_d2, _)| d2 < prev_d2) {
                    best_spike = Some((d2, src_w));
                }
            }

            // Resolve source: prefer a tile when both are in range
            // (tiles get consumed, spikes don't, so this is the
            // "expected" gameplay sink). Source-scale is the seed
            // dimension that the spike's width/height scale from.
            let source_scale: f32 = if let Some((tile_entity, _, tile_radius)) = best_tile {
                if let Ok(mut em) = commands.get_entity(tile_entity) {
                    em.despawn();
                }
                // Tile fuel: scale from tile radius vs base trail
                // radius. Clamped to >= 1.0 so unusually small tiles
                // don't produce sub-default spikes.
                (tile_radius / GLACIER_TRAIL_RADIUS).max(1.0)
            } else if let Some((_, src_width)) = best_spike {
                // Spike chain: each child spike is 70% of the parent's
                // scale. No clamp — successive chains shrink (0.7,
                // 0.49, 0.34, …) which is the intended drop-off so
                // chained spikes can't spiral up into terrain.
                const SPIKE_CHAIN_SCALE: f32 = 0.7;
                (src_width / FROST_SPIKE_BASE_WIDTH) * SPIKE_CHAIN_SCALE
            } else {
                // Nothing matched — drop the message.
                continue;
            };

            let spike_w = FROST_SPIKE_BASE_WIDTH * source_scale;
            let spike_h = FROST_SPIKE_BASE_HEIGHT * source_scale;
            let damage = FROST_SPIKE_BASE_DAMAGE * source_scale
                + FROST_SPIKE_DAMAGE_PER_CHARGE * msg.captured_charge;
            // Cubic mass scaling so a 2× spike is 8× heavier — keeps
            // big spikes from being pushed back by anything reasonable.
            let mass = FROST_SPIKE_BASE_MASS * source_scale * source_scale * source_scale;

            // Final resting position: the spike's *base* sits at the
            // hit point minus a small inset along the normal, so the
            // cuboid clearly grows out of the surface. Center is at
            // hit_pos + normal * (half_h - inset).
            // Initial position: one full spike_h back along the
            // normal, so the rise covers a full height of travel.
            let rest_pos = origin + normal * (spike_h * 0.5 - FROST_SPIKE_GROUND_INSET);
            let rise_distance = spike_h;
            let start_pos = rest_pos - normal * rise_distance;
            let rise_velocity = rise_distance / FROST_SPIKE_RISE_DURATION;

            // Orient the cuboid so its local +Y maps to the surface
            // normal. `disc_rotation` is portal.rs' helper for the
            // same job — reuse rather than re-derive.
            let rotation = crate::spells::portal::disc_rotation(normal);

            let shape = PropShape::Cuboid {
                x: spike_w,
                y: spike_h,
                z: spike_w,
            };
            let collider = Collider::cuboid(spike_w, spike_h, spike_w);
            let net_id = id_alloc.next();
            commands.spawn((
                Name::new(format!("FrostSpike({net_id})")),
                NetworkedProp {
                    shape,
                    tint_seed: 0.0,
                    mass,
                    tint: [0.65, 0.9, 1.0],
                },
                NetworkedId(net_id),
                NetworkedPosition::from_vec3(start_pos),
                Transform::from_translation(start_pos).with_rotation(rotation),
                Position(start_pos),
                Rotation(rotation),
                LinearVelocity(normal * rise_velocity),
                (
                    // Kinematic: avian moves it via LinearVelocity and
                    // uses that velocity in collision response, so the
                    // rising spike physically punts dynamic bodies it
                    // hits (rolling glaciers, fireballs). `settle_frost_spike`
                    // converts to Static once the rise ends.
                    RigidBody::Kinematic,
                    collider,
                    Mass(mass),
                    // Lock rotation so the spike doesn't tumble; with
                    // arbitrary `normal` we can't axis-align translation
                    // locks, but Kinematic doesn't receive collision
                    // pushback so the LinearVelocity we set IS the
                    // motion.
                    LockedAxes::ROTATION_LOCKED,
                    Friction::new(0.6),
                    Restitution::new(0.0),
                    CollisionLayers::new(GameLayer::Default, LayerMask::ALL),
                ),
                FrostSpike {
                    lifetime: FROST_SPIKE_LIFETIME,
                    rise_remaining: FROST_SPIKE_RISE_DURATION,
                    damage,
                    caster,
                },
                // Replicated marker so clients can query "this prop
                // is a spike" — the server-only `FrostSpike` above
                // can't help the client raycast/HUD logic. The
                // `normal` carries the spike's emergence axis so the
                // client-side smoother applies the right rotation
                // instead of the yaw-only default.
                NetworkedFrostSpike { normal: [normal.x, normal.y, normal.z] },
                lightyear::prelude::Replicate::manual(current_senders.clone()),
            ));
        }
    }
}

/// Helper: spawn one frozen-ground tile sitting on top of the surface
/// at `pos`. The tile is a **sensor** — it has a collider so contact
/// events are emitted (future "ball rolls onto ice → grow" hook) but
/// no physical response, so the rolling glacier glides across the
/// surface instead of bouncing on a thick disc.
///
/// `pos` is expected to be the actual surface point (the hit position
/// from the trail raycast). The cylinder is offset upward by half its
/// height so its bottom rests at the surface — without this the disc
/// would be half-buried in the floor.
pub fn spawn_frozen_ground(
    commands: &mut Commands,
    id_alloc: &mut crate::net::server::NetworkedIdAlloc,
    pos: Vec3,
    radius: f32,
    creator: Option<Entity>,
    senders: &[Entity],
) {
    let net_id = id_alloc.next();
    const DISC_THICKNESS: f32 = 0.06;
    let tile_pos = Vec3::new(pos.x, pos.y + DISC_THICKNESS * 0.5 + 0.001, pos.z);
    commands.spawn((
        Name::new(format!("FrozenGround({net_id})")),
        NetworkedFrozenGround { radius },
        NetworkedId(net_id),
        NetworkedPosition::from_vec3(tile_pos),
        Transform::from_translation(tile_pos),
        Position(tile_pos),
        (
            RigidBody::Static,
            Collider::cylinder(radius, DISC_THICKNESS),
            Sensor,
            CollisionLayers::new(GameLayer::Ground, LayerMask::ALL),
        ),
        FrozenGroundLife { age: 0.0, creator },
        lightyear::prelude::Replicate::manual(senders.to_vec()),
    ));
}

// --- Client-side replication hook -----------------------------------------

/// Client: drain `GlacierScaleBroadcast` and apply the new scale to the
/// matching glacier ball's `Transform`. Looks the entity up by
/// `NetworkedId` since lightyear entity ids differ per peer. Mesh +
/// collider both inherit Transform.scale, so the visual grows and the
/// local static collider on the prop matches.
pub fn apply_glacier_scale_broadcasts(
    mut receivers: Query<&mut MessageReceiver<GlacierScaleBroadcast>>,
    mut props: Query<(&NetworkedId, &mut Transform), With<NetworkedProp>>,
) {
    for mut receiver in &mut receivers {
        for msg in receiver.receive() {
            for (id, mut tf) in &mut props {
                if id.0 == msg.net_id {
                    tf.scale = Vec3::splat(msg.scale.max(0.01));
                    break;
                }
            }
        }
    }
}

/// Render a freshly-replicated `NetworkedFrozenGround` as a low cylinder
/// with the cold-glow ice material. Static collider on the client too so
/// the local player can stand on it and feel the slip.
pub fn on_frozen_ground_replicated(
    trigger: On<Add, NetworkedFrozenGround>,
    replicated: Query<
        (&NetworkedFrozenGround, &NetworkedPosition),
        With<lightyear::prelude::Replicated>,
    >,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let Ok((tile, netpos)) = replicated.get(trigger.entity) else {
        return;
    };
    let mesh = meshes.add(Cylinder::new(tile.radius, 0.06));
    let material = materials.add(StandardMaterial {
        base_color: Color::srgba(0.55, 0.85, 1.0, 0.85),
        emissive: LinearRgba::new(0.25, 0.5, 0.9, 1.0),
        perceptual_roughness: 0.1,
        ..default()
    });
    let world_pos = Vec3::new(netpos.x, netpos.y, netpos.z);
    commands.entity(trigger.entity).insert((
        Name::new("RemoteFrozenGround"),
        Mesh3d(mesh),
        MeshMaterial3d(material),
        // Tiles are static; pulling the position straight out of
        // `NetworkedPosition` is enough.
        Transform::from_translation(world_pos),
        Visibility::default(),
        RigidBody::Static,
        Collider::cylinder(tile.radius, 0.06),
        // Sensor on the client too so the mesh shows but doesn't
        // physically obstruct the player walking through.
        Sensor,
        CollisionLayers::new(GameLayer::Ground, LayerMask::ALL),
    ));
}

// --- Client-side HUD indicator -------------------------------------------

/// Small dot below the crosshair that tells the player whether their
/// current aim is a valid frost-spire target. Shown only when the
/// active spell is `frost_spire`. Bright cyan when the raycast lands
/// on `NetworkedFrozenGround`, dim red otherwise. The cast logic in
/// `frost_spire_handler` runs the same raycast at trigger time, so
/// this dot is the player's authoritative preview of whether clicking
/// would actually fire.
struct IceClientHudPlugin;

impl Plugin for IceClientHudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostStartup, spawn_frost_spire_indicator)
            .add_systems(Update, update_frost_spire_indicator);
    }
}

#[derive(Component)]
struct FrostSpireIndicator;

/// Position below the crosshair — far enough not to clash with the
/// charge bar at TOP_PX=124 in `hud.rs`, close enough that the eye
/// catches it without leaving the reticle.
const INDICATOR_OFFSET_PX: f32 = 30.0;
const INDICATOR_SIZE_PX: f32 = 10.0;

fn spawn_frost_spire_indicator(
    mut commands: Commands,
    center: Option<Single<Entity, With<crate::ui::hud::HudCenter>>>,
) {
    // Headless / no-HUD context (test harness, server) — skip.
    let Some(center) = center else { return };
    commands.entity(*center).with_children(|c| {
        c.spawn((
            FrostSpireIndicator,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(-INDICATOR_SIZE_PX * 0.5),
                top: Val::Px(INDICATOR_OFFSET_PX),
                width: Val::Px(INDICATOR_SIZE_PX),
                height: Val::Px(INDICATOR_SIZE_PX),
                display: Display::None,
                ..default()
            },
            BackgroundColor(Color::srgba(0.4, 0.1, 0.15, 0.85)),
        ));
    });
}

fn update_frost_spire_indicator(
    spatial: SpatialQuery,
    active: Option<Single<&crate::spells::ActiveSpell, With<LocalPlayer>>>,
    cam: Option<Single<&GlobalTransform, (With<crate::player::PlayerCamera>, With<LocalPlayer>)>>,
    player_entity: Option<Single<Entity, (With<Player>, With<LocalPlayer>)>>,
    frozen_tiles: Query<Entity, With<NetworkedFrozenGround>>,
    spikes: Query<Entity, With<NetworkedFrostSpike>>,
    mut indicator: Query<(&mut Node, &mut BackgroundColor), With<FrostSpireIndicator>>,
) {
    let mut hide = || {
        for (mut node, _) in &mut indicator {
            node.display = Display::None;
        }
    };
    let (Some(active), Some(cam), Some(player_entity)) = (active, cam, player_entity) else {
        hide();
        return;
    };
    if !active.0.is("frost_spire") {
        hide();
        return;
    }

    let cam_pos = cam.translation();
    let forward = cam.rotation() * Vec3::NEG_Z;
    let Ok(dir) = Dir3::new(forward) else { hide(); return };
    let filter = SpatialQueryFilter::from_excluded_entities(
        std::iter::once(*player_entity).collect::<Vec<_>>(),
    );
    let valid = spatial
        .cast_ray(cam_pos, dir, FROST_SPIRE_RANGE, true, &filter)
        .map_or(false, |hit| {
            frozen_tiles.get(hit.entity).is_ok() || spikes.get(hit.entity).is_ok()
        });

    for (mut node, mut color) in &mut indicator {
        node.display = Display::Flex;
        color.0 = if valid {
            Color::srgba(0.45, 0.85, 1.0, 0.95)
        } else {
            Color::srgba(0.35, 0.1, 0.15, 0.7)
        };
    }
}

// Quiet imports the client-only code uses that the server compile
// would otherwise flag.
#[allow(dead_code)]
fn _unused() {
    let _ = (LocalId(PeerId::Local(0)),);
    let _ = std::marker::PhantomData::<(NetworkedPlayer, PlayerRig)>;
}
