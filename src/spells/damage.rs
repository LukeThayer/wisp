//! Damage system: hurtboxes, hitboxes, AoE damage, and death.
//!
//! Server-authoritative throughout — clients only see the resulting
//! `NetworkedHealth` component and the despawn / respawn that follows
//! `DeathEvent`. The design lives entirely on the server so attribution,
//! team rules, and respawn logic don't need a per-client mirror.
//!
//! Three damage paths converge on `apply_damage_to_hurtbox`:
//! 1. **Contact** — `apply_contact_damage` (regular system) reads
//!    `CollisionStart` and matches Hitbox <-> Hurtbox pairs.
//! 2. **Area** — `apply_aoe_damage` (exclusive, called from
//!    `engine::run_effect` for `EffectDef::AreaDamage`).
//! 3. **Hitscan / beam** — `apply_beam_impulses` in `src/net/server.rs`
//!    calls `apply_damage` directly when the ray hits a `Hurtbox`.
//!
//! Players get a `Hurtbox + NetworkedHealth` on spawn (server side, in
//! `sync_networked_players`). `DeathEvent` for a `NetworkedPlayer`
//! respawns; for anything else, the entity despawns.

use avian3d::prelude::{CollisionStart, LinearVelocity, Position};
use bevy::ecs::system::SystemState;
use bevy::prelude::*;
use serde_json::json;

use crate::net::protocol::{NetworkedHealth, NetworkedPlayer, NetworkedPosition, NetworkOwner};
use crate::trace;

/// Player starting + max hp. Hardcoded for v1 — bump to per-class
/// authoring (BodyDef / SpellDef) when we have multiple entity classes
/// with different hp pools.
pub const PLAYER_MAX_HP: f32 = 100.0;

/// Round-robin respawn points used by the player death handler. Picked
/// up by `NetworkOwner.0 % SPAWN_POINTS.len()` so each client respawns
/// in roughly the same place across deaths.
pub const SPAWN_POINTS: &[Vec3] = &[
    Vec3::new(-6.0, 1.5, 8.0),
    Vec3::new(-2.0, 1.5, 8.0),
    Vec3::new(2.0, 1.5, 8.0),
    Vec3::new(6.0, 1.5, 8.0),
];

/// Team / faction tag carried by both Hurtbox and Hitbox. Same-team
/// contact is no-op. `Neutral` damages every team (and is damaged by
/// every team). `Players` is the default for connected clients.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Team {
    Players,
    Hostile,
    Neutral,
}

impl Team {
    /// True when `attacker` (the hitbox's team) can damage `victim`
    /// (the hurtbox's team). Same-team contact is rejected. Neutral on
    /// either side opts in.
    pub fn can_damage(attacker: Team, victim: Team) -> bool {
        if attacker == Team::Neutral || victim == Team::Neutral {
            return true;
        }
        attacker != victim
    }
}

/// "I can take damage" capability. Lives only on the server — clients
/// see the public-facing `NetworkedHealth` component (which mirrors
/// `hp` + `max_hp` from this struct).
#[derive(Component, Clone, Debug)]
pub struct Hurtbox {
    pub hp: f32,
    pub max_hp: f32,
    pub team: Team,
}

impl Hurtbox {
    pub fn new(max_hp: f32, team: Team) -> Self {
        Self { hp: max_hp, max_hp, team }
    }
}

/// "I deal damage on contact" capability. Attach to projectile bodies,
/// melee swing volumes, or any other entity whose collisions should
/// hurt. The contact-damage system reads this on every `CollisionStart`.
///
/// `source` is the originating caster's entity for kill attribution;
/// `single_hit` despawns the hitbox carrier after its first successful
/// damage event so a fireball can't keep ticking damage while it
/// bounces.
#[derive(Component, Clone, Debug)]
pub struct Hitbox {
    pub damage: f32,
    pub team: Team,
    pub source: Option<Entity>,
    pub single_hit: bool,
}

/// Falloff curve used by `EffectDef::AreaDamage`. Linear from full
/// damage at the origin down to zero at the edge of the spell's
/// area-shape radius. `Flat` opts out of falloff.
#[derive(serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FalloffKind {
    Linear,
    Flat,
}

impl FalloffKind {
    pub fn scale(self, distance: f32, radius: f32) -> f32 {
        match self {
            FalloffKind::Flat => 1.0,
            FalloffKind::Linear => {
                if radius <= 0.0 {
                    1.0
                } else {
                    (1.0 - (distance / radius)).clamp(0.0, 1.0)
                }
            }
        }
    }
}

/// Emitted when an entity's hp drops to zero. Subscribers route the
/// response per entity class: NetworkedPlayer -> respawn, everything
/// else -> despawn. `killer` carries the `Hitbox.source` (or the
/// original caster propagated through child casts) so future kill
/// feed / scoring systems can credit the kill without re-querying.
#[derive(Message, Clone, Debug)]
pub struct DeathEvent {
    pub entity: Entity,
    pub killer: Option<Entity>,
}

/// Wires the damage subsystems: events, systems, observers. Stand-alone
/// from the cast engine so the server bin pulls only what it needs.
pub struct DamagePlugin;

impl Plugin for DamagePlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<DeathEvent>().add_systems(
            Update,
            (apply_contact_damage, handle_deaths).chain(),
        );
    }
}

/// Mutate a hurtbox in place, mirror into NetworkedHealth, and emit a
/// DeathEvent on lethal damage. Shared core for every damage path so
/// the trace/hp-mirror/death logic only lives in one place.
///
/// Returns true iff this hit was lethal. Callers can use the return
/// value to short-circuit follow-up effects, but most don't need to.
fn apply_damage_to_hurtbox(
    target: Entity,
    hurtbox: &mut Hurtbox,
    net_hp: Option<Mut<NetworkedHealth>>,
    amount: f32,
    source: Option<Entity>,
    deaths: &mut Messages<DeathEvent>,
) -> bool {
    if amount <= 0.0 || !amount.is_finite() {
        return false;
    }
    if hurtbox.hp <= 0.0 {
        return false;
    }
    hurtbox.hp = (hurtbox.hp - amount).max(0.0);
    let new_hp = hurtbox.hp;
    if let Some(mut net_hp) = net_hp {
        net_hp.hp = new_hp;
        net_hp.max_hp = hurtbox.max_hp;
    }
    trace::event(
        "damage_applied",
        json!({
            "target": format!("{:?}", target),
            "source": source.map(|e| format!("{:?}", e)),
            "amount": amount,
            "hp_after": new_hp,
        }),
    );
    if new_hp <= 0.0 {
        deaths.write(DeathEvent { entity: target, killer: source });
        true
    } else {
        false
    }
}

/// Apply `amount` damage to `target` from a `&mut World` context (the
/// path used by exclusive-world callers: AoE handlers, beam impulses
/// from system-state, etc.). Idempotent if the target lacks a Hurtbox.
///
/// Uses a `SystemState` to fetch Hurtbox/NetworkedHealth/Messages
/// together so all borrows root through a single ECS access.
pub fn apply_damage(
    world: &mut World,
    target: Entity,
    amount: f32,
    source: Option<Entity>,
) {
    let mut sys_state: SystemState<(
        Query<(&mut Hurtbox, Option<&mut NetworkedHealth>)>,
        ResMut<Messages<DeathEvent>>,
    )> = SystemState::new(world);
    let (mut q, mut deaths) = sys_state.get_mut(world);
    let Ok((mut hurtbox, net_hp)) = q.get_mut(target) else { return; };
    apply_damage_to_hurtbox(
        target,
        &mut hurtbox,
        net_hp,
        amount,
        source,
        &mut deaths,
    );
}

/// Drains `CollisionStart` messages and applies hitbox -> hurtbox
/// damage. Either collider in the pair may carry the hitbox; we probe
/// both. The hitbox carrier despawns when `single_hit` is true and
/// damage was actually applied (a same-team contact doesn't consume the
/// hitbox).
fn apply_contact_damage(
    mut collisions: MessageReader<CollisionStart>,
    hitboxes: Query<&Hitbox>,
    mut hurtboxes: Query<(&mut Hurtbox, Option<&mut NetworkedHealth>)>,
    mut deaths: ResMut<Messages<DeathEvent>>,
    mut commands: Commands,
) {
    for ev in collisions.read() {
        // Either side may carry the hitbox. The other must carry the
        // hurtbox; if both sides have both, the first match wins.
        let pair = match (
            hitboxes.get(ev.collider1).ok(),
            hitboxes.get(ev.collider2).ok(),
            hurtboxes.contains(ev.collider1),
            hurtboxes.contains(ev.collider2),
        ) {
            (Some(_), _, _, true) => Some((ev.collider1, ev.collider2)),
            (_, Some(_), true, _) => Some((ev.collider2, ev.collider1)),
            _ => None,
        };
        let Some((hitbox_entity, hurtbox_entity)) = pair else { continue; };

        let Ok(hitbox) = hitboxes.get(hitbox_entity) else { continue; };
        let hitbox = hitbox.clone();
        if !Team::can_damage(hitbox.team, lookup_team(&hurtboxes, hurtbox_entity)) {
            continue;
        }
        if hitbox.source == Some(hurtbox_entity) {
            continue;
        }
        let Ok((mut hurtbox, net_hp)) = hurtboxes.get_mut(hurtbox_entity) else { continue; };
        let _ = apply_damage_to_hurtbox(
            hurtbox_entity,
            &mut hurtbox,
            net_hp,
            hitbox.damage,
            hitbox.source,
            &mut deaths,
        );
        if hitbox.single_hit {
            if let Ok(mut em) = commands.get_entity(hitbox_entity) {
                em.despawn();
            }
        }
    }
}

fn lookup_team(
    hurtboxes: &Query<(&mut Hurtbox, Option<&mut NetworkedHealth>)>,
    entity: Entity,
) -> Team {
    hurtboxes
        .get(entity)
        .map(|(h, _)| h.team)
        .unwrap_or(Team::Neutral)
}

/// Spatial-query the spell's `TargetDef::Area` and apply damage to every
/// entity in range that has a `Hurtbox`. Linear falloff scaled by the
/// area radius. `source_team` gates friendly fire.
///
/// Called from spells that route through `EffectDef::AreaDamage` and
/// from bespoke handlers that already have the origin in hand (the
/// explosion handler also calls this).
pub fn apply_aoe_damage(
    world: &mut World,
    origin: Vec3,
    radius: f32,
    base_damage: f32,
    falloff: FalloffKind,
    source: Option<Entity>,
    source_team: Team,
) {
    if radius <= 0.0 || base_damage <= 0.0 || !base_damage.is_finite() {
        return;
    }
    // Two phases: collect candidates (read-only), then apply via the
    // shared world path. Splitting avoids a Hurtbox borrow that would
    // collide with apply_damage's SystemState.
    let mut hits: Vec<(Entity, f32)> = Vec::new();
    {
        let mut sys_state: SystemState<Query<(Entity, &GlobalTransform, &Hurtbox)>> =
            SystemState::new(world);
        let q = sys_state.get(world);
        for (entity, tf, hurtbox) in q.iter() {
            if hurtbox.hp <= 0.0 {
                continue;
            }
            if !Team::can_damage(source_team, hurtbox.team) {
                continue;
            }
            let d = (tf.translation() - origin).length();
            if d <= radius {
                hits.push((entity, d));
            }
        }
    }
    for (entity, distance) in hits {
        let scale = falloff.scale(distance, radius);
        let dmg = base_damage * scale;
        if dmg > 0.0 {
            apply_damage(world, entity, dmg, source);
        }
    }
}

/// Drains `DeathEvent` and routes per entity class:
/// - `NetworkedPlayer`: refill hp, snap Position to a spawn point,
///   zero velocity. The replication round-trip carries the new pose
///   to every client.
/// - Anything else: despawn. Component replication handles client-side
///   removal.
fn handle_deaths(
    mut deaths: MessageReader<DeathEvent>,
    mut players: Query<
        (
            &NetworkOwner,
            &mut Hurtbox,
            &mut NetworkedHealth,
            &mut Transform,
            Option<&mut Position>,
            Option<&mut LinearVelocity>,
            Option<&mut NetworkedPosition>,
        ),
        With<NetworkedPlayer>,
    >,
    mut commands: Commands,
) {
    for ev in deaths.read() {
        if let Ok((
            owner,
            mut hurtbox,
            mut net_hp,
            mut tf,
            avian_pos,
            linvel,
            netpos,
        )) = players.get_mut(ev.entity)
        {
            let spawn = SPAWN_POINTS[(owner.0 as usize) % SPAWN_POINTS.len()];
            hurtbox.hp = hurtbox.max_hp;
            net_hp.hp = hurtbox.max_hp;
            net_hp.max_hp = hurtbox.max_hp;
            tf.translation = spawn;
            if let Some(mut p) = avian_pos { p.0 = spawn; }
            if let Some(mut v) = linvel { v.0 = Vec3::ZERO; }
            if let Some(mut np) = netpos {
                np.x = spawn.x;
                np.y = spawn.y;
                np.z = spawn.z;
            }
            trace::event(
                "player_respawned",
                json!({
                    "entity": format!("{:?}", ev.entity),
                    "client_id": owner.0,
                    "pos": [spawn.x, spawn.y, spawn.z],
                    "killer": ev.killer.map(|e| format!("{:?}", e)),
                }),
            );
        } else {
            trace::event(
                "entity_died",
                json!({
                    "entity": format!("{:?}", ev.entity),
                    "killer": ev.killer.map(|e| format!("{:?}", e)),
                }),
            );
            if let Ok(mut em) = commands.get_entity(ev.entity) {
                em.despawn();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_with_damage_plugin() -> App {
        let mut app = App::new();
        app.add_plugins(DamagePlugin);
        app
    }

    #[test]
    fn team_can_damage_rejects_same_team_and_allows_different() {
        assert!(!Team::can_damage(Team::Players, Team::Players));
        assert!(Team::can_damage(Team::Players, Team::Hostile));
        assert!(Team::can_damage(Team::Hostile, Team::Players));
        assert!(Team::can_damage(Team::Players, Team::Neutral));
        assert!(Team::can_damage(Team::Neutral, Team::Players));
    }

    #[test]
    fn falloff_linear_scales_to_zero_at_edge() {
        assert!((FalloffKind::Linear.scale(0.0, 3.0) - 1.0).abs() < 1e-6);
        assert!((FalloffKind::Linear.scale(1.5, 3.0) - 0.5).abs() < 1e-6);
        assert!(FalloffKind::Linear.scale(3.0, 3.0) <= 1e-6);
        assert!(FalloffKind::Linear.scale(4.0, 3.0) <= 1e-6);
        assert!((FalloffKind::Flat.scale(2.0, 3.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn apply_damage_reduces_hp_and_mirrors_into_networked_health() {
        let mut app = world_with_damage_plugin();
        let target = app
            .world_mut()
            .spawn((
                Hurtbox::new(100.0, Team::Players),
                NetworkedHealth { hp: 100.0, max_hp: 100.0 },
            ))
            .id();
        apply_damage(app.world_mut(), target, 30.0, None);
        let hb = app.world().get::<Hurtbox>(target).unwrap();
        assert!((hb.hp - 70.0).abs() < 1e-6, "hp = {}", hb.hp);
        let nh = app.world().get::<NetworkedHealth>(target).unwrap();
        assert!((nh.hp - 70.0).abs() < 1e-6, "net_hp = {}", nh.hp);
    }

    #[test]
    fn apply_damage_emits_death_event_on_hp_zero() {
        let mut app = world_with_damage_plugin();
        let target = app
            .world_mut()
            .spawn(Hurtbox::new(10.0, Team::Players))
            .id();
        let killer = app.world_mut().spawn_empty().id();
        apply_damage(app.world_mut(), target, 50.0, Some(killer));
        let mut sys_state: SystemState<MessageReader<DeathEvent>> =
            SystemState::new(app.world_mut());
        let mut reader = sys_state.get_mut(app.world_mut());
        let events: Vec<_> = reader.read().cloned().collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity, target);
        assert_eq!(events[0].killer, Some(killer));
    }

    #[test]
    fn apply_damage_clamps_at_zero_and_does_not_refire_death() {
        let mut app = world_with_damage_plugin();
        let target = app
            .world_mut()
            .spawn(Hurtbox::new(10.0, Team::Players))
            .id();
        apply_damage(app.world_mut(), target, 50.0, None);
        apply_damage(app.world_mut(), target, 50.0, None);
        let hb = app.world().get::<Hurtbox>(target).unwrap();
        assert_eq!(hb.hp, 0.0);
        let mut sys_state: SystemState<MessageReader<DeathEvent>> =
            SystemState::new(app.world_mut());
        let mut reader = sys_state.get_mut(app.world_mut());
        let events: Vec<_> = reader.read().cloned().collect();
        assert_eq!(events.len(), 1, "over-kill must not re-emit DeathEvent");
    }

    #[test]
    fn apply_aoe_damage_respects_falloff_and_team_gate() {
        let mut app = world_with_damage_plugin();
        let near = app
            .world_mut()
            .spawn((
                Hurtbox::new(100.0, Team::Players),
                Transform::from_xyz(0.0, 0.0, 0.0),
                GlobalTransform::from_xyz(0.0, 0.0, 0.0),
            ))
            .id();
        let far = app
            .world_mut()
            .spawn((
                Hurtbox::new(100.0, Team::Players),
                Transform::from_xyz(2.5, 0.0, 0.0),
                GlobalTransform::from_xyz(2.5, 0.0, 0.0),
            ))
            .id();
        let teammate = app
            .world_mut()
            .spawn((
                Hurtbox::new(100.0, Team::Hostile),
                Transform::from_xyz(0.1, 0.0, 0.0),
                GlobalTransform::from_xyz(0.1, 0.0, 0.0),
            ))
            .id();

        apply_aoe_damage(
            app.world_mut(),
            Vec3::ZERO,
            3.0,
            40.0,
            FalloffKind::Linear,
            None,
            Team::Hostile,
        );
        let near_hp = app.world().get::<Hurtbox>(near).unwrap().hp;
        let far_hp = app.world().get::<Hurtbox>(far).unwrap().hp;
        let mate_hp = app.world().get::<Hurtbox>(teammate).unwrap().hp;
        assert!((near_hp - 60.0).abs() < 1e-4, "near hp = {near_hp}");
        assert!(
            (far_hp - (100.0 - 40.0 / 6.0)).abs() < 1e-4,
            "far hp = {far_hp}"
        );
        assert!((mate_hp - 100.0).abs() < 1e-6, "teammate hp = {mate_hp}");
    }
}
