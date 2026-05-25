//! Iris: variable-aperture charge-and-release lens. Hold Fire to charge from
//! `LensPower`; release to dump the accumulated charge as a single
//! high-impulse burst plus a brief beam flash.
//!
//! Input + charge state live in the cast engine. This module owns the
//! release-time impulse (reading `CastInstance.captured_charge`) and the
//! post-release flash visual.

use avian3d::prelude::*;
use bevy::prelude::*;
use lightyear::prelude::MessageSender;

use crate::input::InputMode;
use crate::net::protocol::{BeamImpulseMessage, PlayerInputChannel};
use crate::player::{LocalPlayer, Player, PlayerRig};
use crate::spells::convex_lens::{
    cast_beam_ray, collect_portal_pairs, rewrite_frustum, Beam, BeamRenderSet, LensPower,
    PortalBeam, BEAM_SEGMENTS,
};
use crate::spells::data::HandlerId;
use crate::spells::engine::{CastEngineSet, CastPhase, CastState};
use crate::spells::handlers::{CastContext, HandlerRegistry};
use crate::spells::markers::Lantern;
use crate::spells::portal::Portal;

const BURST_CAST_ID: &str = "iris.burst";

pub fn register(app: &mut App) {
    app.world_mut()
        .resource_mut::<HandlerRegistry>()
        .register(HandlerId(BURST_CAST_ID.to_string()), burst_handler);
    app.add_plugins(IrisPlugin);
}

/// No-op: see [`apply_burst_on_release`] for the actual burst logic.
fn burst_handler(_world: &mut World, _ctx: &CastContext) {}

struct IrisPlugin;

impl Plugin for IrisPlugin {
    fn build(&self, app: &mut App) {
        // No HUD module — the generic charge bar in `spells::hud` covers
        // the charging visual. The bar snaps to empty on release (no drain
        // animation across the 0.12s flash); acceptable trade-off for
        // sharing the widget with every charged cast.
        //
        // `apply_burst_on_release` must observe the Releasing phase that
        // `phase_advance` writes, then run before `dispatch_casts` clears
        // it. .after + .before sandwich it inside the engine pipeline
        // explicitly; without both constraints, Bevy can schedule it
        // outside the window and the burst silently no-ops.
        app.add_systems(
            Update,
            (
                apply_burst_on_release
                    .after(CastEngineSet::PhaseAdvance)
                    .before(CastEngineSet::Dispatch),
                // Run after convex_lens's `cast_beam` so the iris flash can
                // override the beam-hidden state it sets when convex_lens
                // isn't channeling. Shared `Beam`/`PortalBeam` visibility.
                drive_iris_flash.after(BeamRenderSet),
            ),
        );
    }
}

/// Post-release flash state. Set by [`apply_burst_on_release`] at the
/// moment of release; consumed by [`drive_iris_flash`] over `burst_timer`
/// seconds.
#[derive(Component, Default, Debug)]
pub struct IrisBurst {
    pub burst_charge: f32,
    pub burst_timer: f32,
}

// --- Tuning ----------------------------------------------------------------

const BEAM_MAX_RANGE: f32 = 30.0;
pub const IRIS_MAX_CHARGE: f32 = 3.0;
const IRIS_BURST_DURATION: f32 = 0.12;
const IRIS_BURST_IMPULSE: f32 = 150.0;
/// Damage per unit of captured charge. Iris caps at `IRIS_MAX_CHARGE = 3.0`,
/// so a full release hits for ~3 × this value. Tuned alongside the impulse
/// number so a fully-stoked iris feels punchy without one-shotting a
/// `Hurtbox` at `PLAYER_MAX_HP`.
const IRIS_BURST_DAMAGE: f32 = 22.0;
const IRIS_BURST_MIN: f32 = 0.05;
const IRIS_BURST_RADIUS: f32 = 0.07;

// --- Release: apply burst impulse + arm the flash --------------------------

/// Each frame, scan players whose `iris.burst` cast just entered Releasing
/// phase and apply the burst impulse + arm the 0.12s flash.
fn apply_burst_on_release(
    spatial: SpatialQuery,
    lanterns: Query<Entity, With<Lantern>>,
    portals: Query<(&Portal, &GlobalTransform)>,
    transforms: Query<&GlobalTransform>,
    mut iris_q: Query<
        (Entity, &PlayerRig, &CastState, &mut IrisBurst, Has<LocalPlayer>),
        With<Player>,
    >,
    mut forces: Query<Forces>,
    mut net_sender: Option<Single<&mut MessageSender<BeamImpulseMessage>>>,
) {
    for (player_entity, rig, cast_state, mut burst, is_local) in &mut iris_q {
        let Some(instance) = cast_state.instances.get(BURST_CAST_ID) else {
            continue;
        };
        if instance.phase != CastPhase::Releasing {
            continue;
        }
        let charge = instance.captured_charge.unwrap_or(0.0);
        if charge < IRIS_BURST_MIN {
            continue;
        }

        burst.burst_charge = charge;
        burst.burst_timer = IRIS_BURST_DURATION;

        let Ok(lens) = transforms.get(rig.lens_anchor) else {
            continue;
        };
        let lens_pos = lens.translation();
        let beam_dir = lens.compute_transform().forward();

        let excluded: Vec<Entity> = lanterns
            .iter()
            .chain(std::iter::once(player_entity))
            .collect();
        let filter = SpatialQueryFilter::from_excluded_entities(excluded);
        let pairs = collect_portal_pairs(&portals);
        let hit = cast_beam_ray(&spatial, lens_pos, beam_dir, BEAM_MAX_RANGE, &filter, &pairs);

        if let Some((dir_at_hit, target)) = hit.impulse_info {
            if let Ok(mut f) = forces.get_mut(target) {
                let impulse = dir_at_hit * IRIS_BURST_IMPULSE * charge;
                f.apply_linear_impulse(impulse);
            }
        }

        // For server-authoritative props (NetworkedProp), ask the server to
        // apply the impulse so every client sees the prop move. We send the
        // pre-portal ray (origin = lens, direction = beam_dir): the server
        // raycasts itself and only acts on NetworkedProps. Local lanterns
        // are already pushed by the local impulse above.
        if is_local {
            if let Some(sender) = net_sender.as_mut() {
                let _ = sender.send::<PlayerInputChannel>(BeamImpulseMessage {
                    origin: [lens_pos.x, lens_pos.y, lens_pos.z],
                    direction: [beam_dir.x, beam_dir.y, beam_dir.z],
                    range: BEAM_MAX_RANGE,
                    magnitude: IRIS_BURST_IMPULSE * charge,
                    damage: IRIS_BURST_DAMAGE * charge,
                });
            }
        }
    }
}

// --- Flash visual (post-release) ------------------------------------------

fn drive_iris_flash(
    time: Res<Time>,
    mode: Res<InputMode>,
    spatial: SpatialQuery,
    mut meshes: ResMut<Assets<Mesh>>,
    mut gizmos: Gizmos,
    transforms: Query<&GlobalTransform>,
    lanterns: Query<(Entity, &GlobalTransform), With<Lantern>>,
    portals: Query<(&Portal, &GlobalTransform)>,
    mut player: Query<
        (Entity, &PlayerRig, &LensPower, &mut IrisBurst),
        (With<Player>, With<LocalPlayer>),
    >,
    mut beam: Single<
        (&mut Transform, &mut Visibility, &Mesh3d),
        (With<Beam>, Without<PortalBeam>),
    >,
    mut portal_beam: Single<
        (&mut Transform, &mut Visibility, &Mesh3d),
        (With<PortalBeam>, Without<Beam>),
    >,
) {
    let (beam_tf, beam_vis, beam_mesh) = &mut *beam;
    let (portal_beam_tf, portal_beam_vis, portal_beam_mesh) = &mut *portal_beam;

    let Ok((player_entity, rig, power, mut burst)) = player.single_mut() else {
        return;
    };

    if *mode != InputMode::Player {
        return;
    }
    if burst.burst_timer <= 0.0 {
        return;
    }

    burst.burst_timer = (burst.burst_timer - time.delta_secs()).max(0.0);

    let Ok(lens) = transforms.get(rig.lens_anchor) else {
        return;
    };
    let lens_pos = lens.translation();
    let beam_dir = lens.compute_transform().forward();

    for source in &power.sources {
        if !source.aligned {
            continue;
        }
        if let Ok((_, tf)) = lanterns.get(source.entity) {
            gizmos.line(tf.translation(), lens_pos, Color::srgb(1.0, 0.85, 0.4));
        }
    }

    let excluded: Vec<Entity> = lanterns
        .iter()
        .map(|(e, _)| e)
        .chain(std::iter::once(player_entity))
        .collect();
    let filter = SpatialQueryFilter::from_excluded_entities(excluded);
    let pairs = collect_portal_pairs(&portals);
    let hit = cast_beam_ray(&spatial, lens_pos, beam_dir, BEAM_MAX_RANGE, &filter, &pairs);

    if let Some(mesh) = meshes.get_mut(&beam_mesh.0) {
        rewrite_frustum(
            mesh,
            IRIS_BURST_RADIUS,
            IRIS_BURST_RADIUS,
            hit.primary_dist,
            BEAM_SEGMENTS,
        );
    }
    beam_tf.translation = lens_pos;
    beam_tf.rotation = Quat::from_rotation_arc(Vec3::NEG_Z, *beam_dir);
    **beam_vis = Visibility::Visible;

    if let Some(bent) = &hit.bent {
        if let Ok(bent_dir) = Dir3::new(bent.direction) {
            if let Some(mesh) = meshes.get_mut(&portal_beam_mesh.0) {
                rewrite_frustum(
                    mesh,
                    IRIS_BURST_RADIUS,
                    IRIS_BURST_RADIUS,
                    bent.length,
                    BEAM_SEGMENTS,
                );
            }
            portal_beam_tf.translation = bent.origin;
            portal_beam_tf.rotation = Quat::from_rotation_arc(Vec3::NEG_Z, *bent_dir);
            **portal_beam_vis = Visibility::Visible;
        } else {
            **portal_beam_vis = Visibility::Hidden;
        }
    } else {
        **portal_beam_vis = Visibility::Hidden;
    }
}

