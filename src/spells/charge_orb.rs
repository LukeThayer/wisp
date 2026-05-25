//! Charge orb: a glowing sphere that appears while a spell is charging.
//! Two surfaces, sharing one visual recipe:
//!
//! 1. **Local (1st-person) orb** — child of the local player's
//!    `LensAnchor`, visible only to the local first-person camera. Driven
//!    directly off the local `CastState`.
//!
//! 2. **Remote (3rd-person) orb** — child of each replicated
//!    `NetworkedPlayer`'s `R_equip_joint` bone, visible to everyone else.
//!    Driven off a `RemoteChargeState` component refilled by
//!    `ChargeStateBroadcast` (server-relayed from the casting client).
//!
//! Both ramp emissive + child PointLight intensity with charge progress
//! and color from the active spell's `tint`. The local LensAnchor orb is
//! kept unchanged from the player's first-person POV; the remote orb is
//! what other peers see on the caster's right hand.

use bevy::prelude::*;

use crate::net::protocol::{
    ChargeStateBroadcast, ChargeStateMessage, NetworkOwner, NetworkedPlayer,
};
use crate::net::replication::RemoteWizardBody;
use crate::player::{LensAnchor, LocalPlayer};
use crate::spells::bodies::ColorDef;
use crate::spells::catalog::SpellCatalog;
use crate::spells::data::{CastId, ChargingDef};
use crate::spells::engine::{CastPhase, CastState};
use crate::spells::{ActiveSpell, SpellId};
use lightyear::prelude::{MessageReceiver, MessageSender};

pub struct ChargeOrbPlugin;

impl Plugin for ChargeOrbPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LocalChargeTx>().add_systems(
            Update,
            (
                send_local_charge_state,
                drain_charge_broadcasts,
                ensure_remote_charge_state,
                spawn_remote_charge_orbs,
                update_remote_charge_orbs,
                despawn_remote_charge_orbs,
                update_local_charge_orb,
            ),
        );
    }
}

// --- Tunables --------------------------------------------------------------

/// Right-hand attachment bone in the wizard rig. Authored as a dedicated
/// "equipment" joint at the right palm — the same anchor the artist
/// intended for handheld items.
const R_EQUIP_BONE: &str = "R_equip_joint";

/// Fallback tint when a spell omits `tint`. Matches the lantern body's warm
/// white-yellow so unset spells don't look broken.
const DEFAULT_TINT: LinearRgba = LinearRgba::new(1.0, 0.95, 0.7, 1.0);

const ORB_RADIUS: f32 = 0.06;
const EMISSIVE_MIN: f32 = 0.4;
const EMISSIVE_MAX: f32 = 8.0;
const LIGHT_MIN: f32 = 800.0;
const LIGHT_MAX: f32 = 60_000.0;

/// Inverse-scale compensation for orbs attached to wizard rig bones. The
/// rig's `BaseMale` root has Blender-export scale 0.01 (the model was
/// authored in centimeters), so a child sphere with default scale 1.0
/// renders at 0.6mm. Multiplying the orb's local scale by 100 cancels the
/// inherited scale and yields the intended worldspace size.
const REMOTE_ORB_SCALE: f32 = 100.0;

fn intensity_curve(ratio: f32) -> f32 {
    let t = ratio.clamp(0.0, 1.0);
    t * t
}

fn lerp(min: f32, max: f32, t: f32) -> f32 {
    min + (max - min) * t
}

fn color_to_linear(c: ColorDef) -> LinearRgba {
    LinearRgba::new(c.r, c.g, c.b, c.a)
}

fn lookup_tint(catalog: &SpellCatalog, spell_id: &SpellId) -> LinearRgba {
    catalog
        .get(spell_id)
        .and_then(|s| s.tint)
        .map(color_to_linear)
        .unwrap_or(DEFAULT_TINT)
}

/// Compute the active spell's first-charging-cast state for a player,
/// matching the HUD charge bar's selection rule. Returns
/// `(cast_id, ratio_0_to_1, tint)` when something is Charging; `None`
/// otherwise.
fn current_charge_state(
    catalog: &SpellCatalog,
    active: &ActiveSpell,
    cast_state: &CastState,
) -> Option<(CastId, f32, LinearRgba)> {
    let spell = catalog.get(&active.0)?;
    let tint = lookup_tint(catalog, &active.0);
    for cast in &spell.casts {
        let ChargingDef::Charging {
            max_charge,
            overcharge,
            ..
        } = &cast.charging
        else {
            continue;
        };
        let instance = cast_state.instances.get(cast.id.0.as_str())?;
        if instance.phase != CastPhase::Charging {
            continue;
        }
        let display_max = overcharge.as_ref().map(|o| o.max).unwrap_or(*max_charge);
        let ratio = (instance.charge / display_max.max(1e-6)).clamp(0.0, 1.0);
        return Some((cast.id.clone(), ratio, tint));
    }
    None
}

// --- Local (1st-person) orb -----------------------------------------------

/// Marker on the local orb root entity. Holds the cast id (to detect mid-
/// charge spell switches) and the child `PointLight` entity (so we don't
/// have to walk children each frame).
#[derive(Component)]
struct ChargeOrb {
    cast_id: CastId,
    light: Entity,
}

fn update_local_charge_orb(
    mut commands: Commands,
    catalog: Res<SpellCatalog>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    player: Option<Single<(&ActiveSpell, &CastState), With<LocalPlayer>>>,
    lens_anchor: Option<Single<Entity, (With<LensAnchor>, With<LocalPlayer>)>>,
    orbs: Query<(Entity, &ChargeOrb, &MeshMaterial3d<StandardMaterial>)>,
    mut lights: Query<&mut PointLight>,
) {
    let Some(player) = player else {
        for (e, _, _) in &orbs {
            commands.entity(e).despawn();
        }
        return;
    };
    let (active, cast_state) = *player;
    let target = current_charge_state(&catalog, active, cast_state);

    let existing = orbs.iter().next();
    match (target, existing) {
        (Some((cast_id, ratio, tint)), Some((orb_entity, orb, mat))) => {
            if orb.cast_id != cast_id {
                commands.entity(orb_entity).despawn();
                return;
            }
            apply_intensity(&mut materials, &mut lights, &mat.0, orb.light, tint, ratio);
        }
        (Some((cast_id, ratio, tint)), None) => {
            let Some(anchor) = lens_anchor else {
                return;
            };
            let (orb, light) = spawn_orb_visual(
                &mut commands,
                &mut meshes,
                &mut materials,
                tint,
                ratio,
            );
            commands
                .entity(orb)
                .insert(ChargeOrb { cast_id, light });
            commands.entity(*anchor).add_child(orb);
        }
        (None, Some((orb_entity, _, _))) => {
            commands.entity(orb_entity).despawn();
        }
        (None, None) => {}
    }
}

// --- Remote (3rd-person) orb ----------------------------------------------

/// Per-player mirror of the latest `ChargeStateBroadcast`. Lives on each
/// `NetworkedPlayer` (added by `ensure_remote_charge_state`). Not
/// replicated — refilled from broadcast messages each tick.
#[derive(Component, Default, Clone, Copy)]
pub struct RemoteChargeState {
    pub active: bool,
    pub ratio: f32,
    pub tint: LinearRgba,
}

/// Marker on the orb entity attached to a remote player's `R_equip_joint`.
/// Tracks back to the owning player so the despawn / update systems know
/// which `RemoteChargeState` drives it.
#[derive(Component)]
struct RemoteChargeOrb {
    player: Entity,
    light: Entity,
}

/// Cached "what we last sent" so we can emit exactly one trailing
/// `active: false` message on transition out of charging (the channel is
/// unreliable, so emitting just one isn't bulletproof, but it's the same
/// pattern the beam path uses and works in practice).
#[derive(Resource, Default)]
struct LocalChargeTx {
    was_active: bool,
}

fn send_local_charge_state(
    catalog: Res<SpellCatalog>,
    mut tx: ResMut<LocalChargeTx>,
    player: Option<Single<(&ActiveSpell, &CastState), With<LocalPlayer>>>,
    sender: Option<Single<&mut MessageSender<ChargeStateMessage>>>,
) {
    let Some(player) = player else {
        return;
    };
    let Some(mut sender) = sender else {
        return;
    };
    let (active, cast_state) = *player;
    let current = current_charge_state(&catalog, active, cast_state);
    match (current, tx.was_active) {
        (Some((_, ratio, tint)), _) => {
            let _ = sender.send::<crate::net::protocol::PlayerInputChannel>(
                ChargeStateMessage {
                    active: true,
                    ratio,
                    tint: [tint.red, tint.green, tint.blue],
                },
            );
            tx.was_active = true;
        }
        (None, true) => {
            let _ = sender.send::<crate::net::protocol::PlayerInputChannel>(
                ChargeStateMessage {
                    active: false,
                    ratio: 0.0,
                    tint: [0.0; 3],
                },
            );
            tx.was_active = false;
        }
        (None, false) => {}
    }
}

fn drain_charge_broadcasts(
    mut receivers: Query<&mut MessageReceiver<ChargeStateBroadcast>>,
    mut players: Query<
        (&NetworkOwner, &mut RemoteChargeState),
        With<NetworkedPlayer>,
    >,
    local_ids: Query<
        &lightyear::prelude::LocalId,
        Without<lightyear::prelude::server::ClientOf>,
    >,
) {
    let my_id = local_ids.iter().next().and_then(|local_id| match local_id.0 {
        lightyear::prelude::PeerId::Netcode(id)
        | lightyear::prelude::PeerId::Steam(id)
        | lightyear::prelude::PeerId::Local(id)
        | lightyear::prelude::PeerId::Entity(id) => Some(id),
        _ => None,
    });
    for mut receiver in &mut receivers {
        for msg in receiver.receive() {
            // Skip self-loopback. The local player's own orb is rendered
            // via the LensAnchor (1st-person) path; their replicated
            // NetworkedPlayer body is hidden in 1st-person anyway by
            // `hide_self_wizard_body`, so a self-tagged remote orb would
            // be invisible regardless — but we filter for clarity.
            if my_id == Some(msg.client_id) {
                continue;
            }
            for (owner, mut state) in &mut players {
                if owner.0 == msg.client_id {
                    state.active = msg.active;
                    state.ratio = msg.ratio;
                    state.tint = LinearRgba::new(msg.tint[0], msg.tint[1], msg.tint[2], 1.0);
                    break;
                }
            }
        }
    }
}

fn ensure_remote_charge_state(
    mut commands: Commands,
    q: Query<Entity, (With<NetworkedPlayer>, Without<RemoteChargeState>)>,
) {
    for e in &q {
        commands.entity(e).insert(RemoteChargeState::default());
    }
}

fn spawn_remote_charge_orbs(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    bones: Query<(Entity, &Name)>,
    parents: Query<&ChildOf>,
    body_marker: Query<(), With<RemoteWizardBody>>,
    players: Query<(Entity, &RemoteChargeState), With<NetworkedPlayer>>,
    existing: Query<&RemoteChargeOrb>,
) {
    // Build the set of players that already have an orb so we don't
    // double-spawn when multiple `R_equip_joint` candidates exist (the rig
    // only has one, but defensively).
    let occupied: std::collections::HashSet<Entity> =
        existing.iter().map(|o| o.player).collect();

    for (bone_entity, name) in &bones {
        if name.as_str() != R_EQUIP_BONE {
            continue;
        }
        let Some(player) = find_remote_player(bone_entity, &parents, &body_marker, &players)
        else {
            continue;
        };
        if occupied.contains(&player) {
            continue;
        }
        let Ok((_, state)) = players.get(player) else {
            continue;
        };
        if !state.active {
            continue;
        }
        let (orb, light) = spawn_orb_visual(
            &mut commands,
            &mut meshes,
            &mut materials,
            state.tint,
            state.ratio,
        );
        commands.entity(orb).insert((
            RemoteChargeOrb { player, light },
            // Compensate the rig's inherited scale so the orb renders at
            // the same worldspace size as the LensAnchor (1st-person) orb.
            Transform::from_scale(Vec3::splat(REMOTE_ORB_SCALE)),
        ));
        commands.entity(bone_entity).add_child(orb);
    }
}

fn update_remote_charge_orbs(
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut lights: Query<&mut PointLight>,
    orbs: Query<(&RemoteChargeOrb, &MeshMaterial3d<StandardMaterial>)>,
    players: Query<&RemoteChargeState, With<NetworkedPlayer>>,
) {
    for (orb, mat) in &orbs {
        let Ok(state) = players.get(orb.player) else {
            continue;
        };
        if !state.active {
            continue;
        }
        apply_intensity(
            &mut materials,
            &mut lights,
            &mat.0,
            orb.light,
            state.tint,
            state.ratio,
        );
    }
}

fn despawn_remote_charge_orbs(
    mut commands: Commands,
    orbs: Query<(Entity, &RemoteChargeOrb)>,
    players: Query<&RemoteChargeState, With<NetworkedPlayer>>,
) {
    for (orb_entity, orb) in &orbs {
        // If the owning player vanished (disconnect) or their charge went
        // inactive, kill the orb. Children of a despawned parent are
        // already cleaned up by Bevy, but explicit despawn handles the
        // "still-here player, inactive charge" case.
        let alive_and_active = players
            .get(orb.player)
            .map(|s| s.active)
            .unwrap_or(false);
        if !alive_and_active {
            commands.entity(orb_entity).despawn();
        }
    }
}

/// Walk from `entity` up the parent chain until we cross a
/// `RemoteWizardBody` and then reach a `NetworkedPlayer` whose
/// `RemoteChargeState` we can query. Returns that player entity.
fn find_remote_player(
    entity: Entity,
    parents: &Query<&ChildOf>,
    body_marker: &Query<(), With<RemoteWizardBody>>,
    players: &Query<(Entity, &RemoteChargeState), With<NetworkedPlayer>>,
) -> Option<Entity> {
    let mut cur = entity;
    let mut under_body = false;
    loop {
        if body_marker.contains(cur) {
            under_body = true;
        }
        if under_body && players.get(cur).is_ok() {
            return Some(cur);
        }
        match parents.get(cur) {
            Ok(p) => cur = p.parent(),
            Err(_) => return None,
        }
    }
}

// --- Shared visual helpers -------------------------------------------------

/// Build the orb root + child light entities at the given charge ratio.
/// Returns `(orb_entity, light_entity)`. Caller is responsible for
/// parenting `orb_entity` to wherever it should live and inserting any
/// per-surface marker components (`ChargeOrb` for local,
/// `RemoteChargeOrb` for remote).
fn spawn_orb_visual(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    tint: LinearRgba,
    ratio: f32,
) -> (Entity, Entity) {
    let curve = intensity_curve(ratio);
    let emissive_scale = lerp(EMISSIVE_MIN, EMISSIVE_MAX, curve);
    let light_intensity = lerp(LIGHT_MIN, LIGHT_MAX, curve);

    let mesh = meshes.add(Sphere::new(ORB_RADIUS));
    let material = materials.add(StandardMaterial {
        base_color: Color::linear_rgba(tint.red * 0.4, tint.green * 0.4, tint.blue * 0.4, 1.0),
        emissive: LinearRgba::new(
            tint.red * emissive_scale,
            tint.green * emissive_scale,
            tint.blue * emissive_scale,
            1.0,
        ),
        perceptual_roughness: 0.4,
        ..default()
    });

    let light = commands
        .spawn((
            Name::new("ChargeOrb-Light"),
            PointLight {
                color: Color::linear_rgba(tint.red, tint.green, tint.blue, 1.0),
                intensity: light_intensity,
                range: 6.0,
                shadows_enabled: false,
                ..default()
            },
            Transform::default(),
        ))
        .id();

    let orb = commands
        .spawn((
            Name::new("ChargeOrb"),
            Mesh3d(mesh),
            MeshMaterial3d(material),
            Transform::default(),
            Visibility::default(),
        ))
        .add_child(light)
        .id();

    (orb, light)
}

fn apply_intensity(
    materials: &mut Assets<StandardMaterial>,
    lights: &mut Query<&mut PointLight>,
    material_handle: &Handle<StandardMaterial>,
    light_entity: Entity,
    tint: LinearRgba,
    ratio: f32,
) {
    let curve = intensity_curve(ratio);
    let emissive_scale = lerp(EMISSIVE_MIN, EMISSIVE_MAX, curve);
    let light_intensity = lerp(LIGHT_MIN, LIGHT_MAX, curve);

    if let Some(material) = materials.get_mut(material_handle) {
        material.emissive = LinearRgba::new(
            tint.red * emissive_scale,
            tint.green * emissive_scale,
            tint.blue * emissive_scale,
            1.0,
        );
    }
    if let Ok(mut light) = lights.get_mut(light_entity) {
        light.intensity = light_intensity;
    }
}
