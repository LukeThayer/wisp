//! Convex Lens spell: while channeling, projects a beam from the wand tip
//! along the player's gaze. Power and convergence depend on the relative
//! position of the player vs. placed lanterns; see [`compute_lens_power`].
//!
//! Owns the shared Beam / PortalBeam visual entities — `iris` reuses them
//! during its burst flash. The lens-power computation is a per-player
//! component, so this module runs the same systems for every controlled
//! player in multiplayer.

use avian3d::prelude::*;
use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use lightyear::prelude::MessageSender;

use crate::input::InputMode;
use crate::net::protocol::{BeamImpulseMessage, PlayerInputChannel};
use crate::player::{LocalPlayer, Player, PlayerRig};
use crate::spells::data::HandlerId;
use crate::spells::engine::{CastPhase, CastState};
use crate::spells::handlers::{CastContext, HandlerRegistry};
use crate::spells::markers::Lantern;
use crate::spells::portal::{Portal, PortalSlot, PORTAL_RADIUS};

/// Display cap for the alignment HUD: how many in-range lanterns we draw
/// dots for. Matches `MaxCount` on the lantern.throw cast in RON.
const MAX_LANTERN_DOTS: usize = 3;

/// Cast id for the convex-lens beam. Matches `assets/spells/convex_lens.spell.ron`.
const BEAM_CAST_ID: &str = "convex_lens.beam";

pub fn register(app: &mut App) {
    // The cast engine routes input + lifecycle for `convex_lens.beam`; the
    // bespoke beam math runs as a regular Bevy system below, gated by
    // `CastState`. The handler itself is a no-op — registering it satisfies
    // the catalog validator and keeps a hook open for future MP semantics.
    app.world_mut()
        .resource_mut::<HandlerRegistry>()
        .register(HandlerId(BEAM_CAST_ID.to_string()), beam_handler);
    app.add_plugins(ConvexLensPlugin);
}

/// No-op: see [`cast_beam`] for the actual per-frame beam logic.
fn beam_handler(_world: &mut World, _ctx: &CastContext) {}

/// Anchor for the shared beam-render slot. `cast_beam` (here) hides the
/// beam when convex_lens isn't channeling; iris's burst flash overrides it
/// for ~0.12s. Without this set, Bevy can schedule the iris flash before
/// `cast_beam` and the hide overwrites the show.
#[derive(SystemSet, Clone, Hash, Eq, PartialEq, Debug)]
pub struct BeamRenderSet;

struct ConvexLensPlugin;

impl Plugin for ConvexLensPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup_beam)
            .add_systems(
                Update,
                (compute_lens_power, cast_beam.in_set(BeamRenderSet)).chain(),
            );
        hud::install(app);
    }
}

// --- Components ------------------------------------------------------------

/// Marker for the convex-lens beam frustum mesh. There's exactly one Beam
/// in the world — it's reused by every player and reshaped each frame for
/// whoever is channeling locally.
#[derive(Component)]
pub struct Beam;

/// Continuation segment for a beam that refracted through a portal pair.
/// Hidden when the beam doesn't touch a portal.
#[derive(Component)]
pub struct PortalBeam;

/// Live truth for "how much power is entering the player's lens right now."
/// Recomputed each frame by [`compute_lens_power`]; read by the beam, the
/// iris charge accumulator, and the HUD overlays.
#[derive(Component, Default, Clone)]
pub struct LensPower {
    /// Sum of contributions from every aligned source. With three aligned
    /// lanterns at point-blank this reaches 3.0; both beam impulse and the
    /// lumen readout scale with this combined value.
    pub scalar: f32,
    /// True iff there's at least one placed lantern within tether range.
    pub in_range: bool,
    /// True iff the active source is within the alignment cone.
    pub aligned: bool,
    pub lens_distance: f32,
    /// Lumens to display on the HUD. Equals `scalar * MAX_LUMENS`.
    pub lumens: f32,
    pub active_lantern: Option<Entity>,
    /// All placed lanterns within tether range. Drives the alignment
    /// indicator's one-dot-per-source view.
    pub sources: Vec<LensSource>,
}

#[derive(Clone, Copy)]
pub struct LensSource {
    pub entity: Entity,
    /// Normalized signed offset; ±1.0 means "at the yaw tolerance boundary."
    pub yaw_offset: f32,
    pub pitch_offset: f32,
    pub aligned: bool,
    pub lens_distance: f32,
}

// --- Tuning ----------------------------------------------------------------

const BEAM_MAX_RANGE: f32 = 30.0;
/// Peak impulse magnitude (kg·m/s) per second of channel at point-blank.
const BEAM_IMPULSE_RATE: f32 = 80.0;
/// Damage applied per second of beam contact at full power
/// (`power.scalar = 1`). Scaled per-frame by `power.scalar * dt` so the
/// hit's lens-power gating modulates damage the same way it modulates
/// impulse. 30/sec → ~3.3s sustained beam to drop a player at
/// PLAYER_MAX_HP, leaving room for evasion / counter-fire.
const BEAM_DAMAGE_RATE: f32 = 30.0;
/// Distance from the lantern at which beam power decays to zero.
const BEAM_POWER_FALLOFF: f32 = 14.0;
/// Past this distance, the channel breaks.
const TETHER_MAX_RANGE: f32 = 25.0;

/// Physical radius of the convex lens, in metres.
const LENS_RADIUS: f32 = 0.10;
/// Lens focal length. Stand at this distance from the lantern for a
/// parallel beam.
const FOCAL_LENGTH: f32 = 3.0;
/// Beam mesh tessellation around its circular cross-section.
pub(crate) const BEAM_SEGMENTS: u32 = 24;

/// Display scale: scalar 1.0 → MAX_LUMENS lumens shown on HUD.
pub const MAX_LUMENS: f32 = 1200.0;
/// Asymmetric alignment tolerance. Wider vertically than horizontally so the
/// player can look up/down at targets without dropping the channel.
const LENS_ALIGNMENT_YAW_DEG: f32 = 15.0;
const LENS_ALIGNMENT_PITCH_DEG: f32 = 35.0;

// --- Beam visual setup -----------------------------------------------------

fn setup_beam(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let mesh = build_frustum_mesh(LENS_RADIUS, LENS_RADIUS, 1.0, BEAM_SEGMENTS);
    let mesh_handle = meshes.add(mesh);
    let material = materials.add(StandardMaterial {
        base_color: Color::srgba(1.0, 0.92, 0.55, 0.55),
        emissive: LinearRgba::rgb(6.0, 5.0, 2.0),
        alpha_mode: AlphaMode::Add,
        unlit: false,
        cull_mode: None,
        double_sided: true,
        ..default()
    });

    commands.spawn((
        Name::new("Beam"),
        Beam,
        Mesh3d(mesh_handle),
        MeshMaterial3d(material.clone()),
        Transform::default(),
        Visibility::Hidden,
    ));

    let portal_mesh = build_frustum_mesh(LENS_RADIUS, LENS_RADIUS, 1.0, BEAM_SEGMENTS);
    let portal_mesh_handle = meshes.add(portal_mesh);
    commands.spawn((
        Name::new("PortalBeam"),
        PortalBeam,
        Mesh3d(portal_mesh_handle),
        MeshMaterial3d(material),
        Transform::default(),
        Visibility::Hidden,
    ));
}

/// Builds a frustum (truncated cone) extending from z=0 (radius `r_start`) to
/// z=-length (radius `r_end`). Oriented in -Z so it lines up with Bevy's "forward."
pub(crate) fn build_frustum_mesh(r_start: f32, r_end: f32, length: f32, segments: u32) -> Mesh {
    use core::f32::consts::TAU;
    let n = segments as usize;
    let mut positions: Vec<[f32; 3]> = Vec::with_capacity(2 * n);
    let mut normals: Vec<[f32; 3]> = Vec::with_capacity(2 * n);
    let mut indices: Vec<u32> = Vec::with_capacity(6 * n);

    for i in 0..segments {
        let angle = i as f32 * TAU / segments as f32;
        let (s, c) = angle.sin_cos();
        positions.push([r_start * c, r_start * s, 0.0]);
        positions.push([r_end * c, r_end * s, -length]);
        normals.push([c, s, 0.0]);
        normals.push([c, s, 0.0]);
    }

    for i in 0..segments {
        let next = (i + 1) % segments;
        let a = 2 * i;
        let b = 2 * i + 1;
        let cc = 2 * next + 1;
        let d = 2 * next;
        indices.extend_from_slice(&[a, b, cc, a, cc, d]);
    }

    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
    mesh.insert_indices(Indices::U32(indices));
    mesh
}

pub(crate) fn rewrite_frustum(mesh: &mut Mesh, r_start: f32, r_end: f32, length: f32, segments: u32) {
    use core::f32::consts::TAU;
    let n = segments as usize;
    let mut positions: Vec<[f32; 3]> = Vec::with_capacity(2 * n);
    let mut normals: Vec<[f32; 3]> = Vec::with_capacity(2 * n);

    for i in 0..segments {
        let angle = i as f32 * TAU / segments as f32;
        let (s, c) = angle.sin_cos();
        positions.push([r_start * c, r_start * s, 0.0]);
        positions.push([r_end * c, r_end * s, -length]);
        normals.push([c, s, 0.0]);
        normals.push([c, s, 0.0]);
    }

    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
}

// --- Lens power computation ------------------------------------------------

/// Recompute every player's `LensPower` from the placed lanterns. Runs every
/// frame; cheap (one query per lantern per player).
pub fn compute_lens_power(
    lanterns: Query<(Entity, &GlobalTransform), With<Lantern>>,
    transforms: Query<&GlobalTransform>,
    mut players: Query<(&PlayerRig, &mut LensPower), With<Player>>,
) {
    let yaw_tol = LENS_ALIGNMENT_YAW_DEG.to_radians();
    let pitch_tol = LENS_ALIGNMENT_PITCH_DEG.to_radians();

    for (rig, mut power) in &mut players {
        let Ok(lens) = transforms.get(rig.lens_anchor) else {
            *power = LensPower::default();
            continue;
        };
        let lens_pos = lens.translation();
        let lens_rot_inv = lens.rotation().inverse();

        let mut sources: Vec<LensSource> = Vec::with_capacity(MAX_LANTERN_DOTS);
        for (entity, lantern_tf) in &lanterns {
            let lantern_pos = lantern_tf.translation();
            let lens_dist = lens_pos.distance(lantern_pos);
            if lens_dist > TETHER_MAX_RANGE {
                continue;
            }

            let (aligned, yaw_norm, pitch_norm) = match Dir3::new(lens_pos - lantern_pos) {
                Ok(radial) => {
                    let local = lens_rot_inv * radial.as_vec3();
                    let pitch = local.y.clamp(-1.0, 1.0).asin();
                    let yaw = local.x.atan2(-local.z);
                    let aligned = yaw.abs() <= yaw_tol && pitch.abs() <= pitch_tol;
                    (aligned, yaw / yaw_tol, pitch / pitch_tol)
                }
                Err(_) => (true, 0.0, 0.0),
            };

            sources.push(LensSource {
                entity,
                yaw_offset: yaw_norm,
                pitch_offset: pitch_norm,
                aligned,
                lens_distance: lens_dist,
            });
        }

        let active_idx = sources
            .iter()
            .enumerate()
            .reduce(|(i_a, a), (i_b, b)| {
                let prefer_b = match (b.aligned, a.aligned) {
                    (true, false) => true,
                    (false, true) => false,
                    _ => b.lens_distance < a.lens_distance,
                };
                if prefer_b { (i_b, b) } else { (i_a, a) }
            })
            .map(|(i, _)| i);

        let Some(active_idx) = active_idx else {
            *power = LensPower::default();
            continue;
        };
        let active = sources[active_idx];

        let scalar: f32 = sources
            .iter()
            .filter(|s| s.aligned)
            .map(|s| (1.0 - s.lens_distance / BEAM_POWER_FALLOFF).clamp(0.0, 1.0))
            .sum();
        let lumens = scalar * MAX_LUMENS;

        *power = LensPower {
            scalar,
            in_range: true,
            aligned: active.aligned,
            lens_distance: active.lens_distance,
            lumens,
            active_lantern: Some(active.entity),
            sources,
        };
    }
}

// --- Beam cast (ConvexLens only) -------------------------------------------

/// Draws the convex-lens beam and applies its continuous impulse. Runs every
/// frame; gates on the cast engine's `CastState` — the beam fires while the
/// local player's `convex_lens.beam` cast is in Channeling phase. The iris
/// spell uses [`cast_beam_ray`] from its own system during the burst flash.
pub fn cast_beam(
    time: Res<Time>,
    mode: Res<InputMode>,
    spatial: SpatialQuery,
    mut gizmos: Gizmos,
    mut meshes: ResMut<Assets<Mesh>>,
    transforms: Query<&GlobalTransform>,
    player: Single<
        (Entity, &PlayerRig, &LensPower, &CastState),
        (With<Player>, With<LocalPlayer>),
    >,
    lanterns: Query<(Entity, &GlobalTransform), With<Lantern>>,
    portals: Query<(&Portal, &GlobalTransform)>,
    mut beam: Single<
        (&mut Transform, &mut Visibility, &Mesh3d),
        (With<Beam>, Without<PortalBeam>),
    >,
    mut portal_beam: Single<
        (&mut Transform, &mut Visibility, &Mesh3d),
        (With<PortalBeam>, Without<Beam>),
    >,
    mut forces: Query<Forces>,
    mut net_sender: Option<Single<&mut MessageSender<BeamImpulseMessage>>>,
) {
    let (beam_tf, beam_vis, beam_mesh) = &mut *beam;
    let (portal_beam_tf, portal_beam_vis, portal_beam_mesh) = &mut *portal_beam;

    let (player_entity, rig, power, cast_state) = *player;

    let channeling = cast_state
        .instances
        .get(BEAM_CAST_ID)
        .map(|i| i.phase == CastPhase::Channeling)
        .unwrap_or(false);

    if *mode != InputMode::Player || !channeling || power.scalar <= 0.0 {
        **beam_vis = Visibility::Hidden;
        **portal_beam_vis = Visibility::Hidden;
        return;
    }

    let Ok(lens) = transforms.get(rig.lens_anchor) else {
        **beam_vis = Visibility::Hidden;
        **portal_beam_vis = Visibility::Hidden;
        return;
    };
    let lens_pos = lens.translation();
    let beam_dir = lens.compute_transform().forward();
    let dt = time.delta_secs();

    let portal_pairs = collect_portal_pairs(&portals);

    // Tether visual from every aligned source.
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
    let hit = cast_beam_ray(
        &spatial,
        lens_pos,
        beam_dir,
        BEAM_MAX_RANGE,
        &filter,
        &portal_pairs,
    );
    let hit_distance = hit.primary_dist;

    // Thin-lens: 1/d_o + 1/d_i = 1/f. d_i > 0 → converge, d_i < 0 → diverge.
    let d_o = power.lens_distance;
    let d_i = if (d_o - FOCAL_LENGTH).abs() < 0.001 {
        f32::INFINITY
    } else {
        FOCAL_LENGTH * d_o / (d_o - FOCAL_LENGTH)
    };

    let (length, r_end) = if d_i.is_finite() && d_i > 0.0 {
        if hit_distance < d_i {
            let r = LENS_RADIUS * (1.0 - hit_distance / d_i).max(0.0);
            (hit_distance, r)
        } else {
            (d_i, 0.0)
        }
    } else if d_i.is_finite() && d_i < 0.0 {
        let length = hit_distance;
        let r = LENS_RADIUS * (1.0 + length / d_i.abs());
        (length, r)
    } else {
        (hit_distance, LENS_RADIUS)
    };

    if let Some(mesh) = meshes.get_mut(&beam_mesh.0) {
        rewrite_frustum(mesh, LENS_RADIUS, r_end, length, BEAM_SEGMENTS);
    }
    beam_tf.translation = lens_pos;
    beam_tf.rotation = Quat::from_rotation_arc(Vec3::NEG_Z, *beam_dir);
    **beam_vis = Visibility::Visible;

    if let Some(bent) = &hit.bent {
        if let Ok(bent_dir) = Dir3::new(bent.direction) {
            if let Some(mesh) = meshes.get_mut(&portal_beam_mesh.0) {
                rewrite_frustum(
                    mesh,
                    LENS_RADIUS,
                    LENS_RADIUS,
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

    let impulse_magnitude = BEAM_IMPULSE_RATE * power.scalar * dt;
    let damage = BEAM_DAMAGE_RATE * power.scalar * dt;
    if let Some((dir_at_hit, target)) = hit.impulse_info {
        // Local impulse — works on locally-simulated bodies (lanterns).
        // Replicated server-authoritative props (NetworkedProp) have
        // `RigidBody::Static` on the client so this is a no-op for them;
        // they get pushed by the server in response to the network
        // message below.
        if let Ok(mut f) = forces.get_mut(target) {
            f.apply_linear_impulse(dir_at_hit * impulse_magnitude);
        }
    }

    // Mirror the impulse over the wire so the server applies it to its
    // authoritative `NetworkedProp` (if any) along the same ray. Result
    // streams back to every client via `NetworkedPosition` replication.
    if let Some(sender) = net_sender.as_mut() {
        let _ = sender.send::<PlayerInputChannel>(BeamImpulseMessage {
            origin: [lens_pos.x, lens_pos.y, lens_pos.z],
            direction: [beam_dir.x, beam_dir.y, beam_dir.z],
            range: BEAM_MAX_RANGE,
            magnitude: impulse_magnitude,
            damage,
        });
    }
}

// --- Beam ray (shared with iris) ------------------------------------------

pub struct BeamHit {
    pub primary_dist: f32,
    pub impulse_info: Option<(Vec3, Entity)>,
    pub bent: Option<BentSegment>,
}

pub struct BentSegment {
    pub origin: Vec3,
    pub direction: Vec3,
    pub length: f32,
}

/// Collect primary↔secondary portal transforms (both directions) for the
/// beam-ray helper. Returns empty when the pair is incomplete.
pub(crate) fn collect_portal_pairs(
    portals: &Query<(&Portal, &GlobalTransform)>,
) -> Vec<(Transform, Transform)> {
    let mut primary_tf: Option<Transform> = None;
    let mut secondary_tf: Option<Transform> = None;
    for (portal, gt) in portals.iter() {
        let tf = gt.compute_transform();
        match portal.slot {
            PortalSlot::Primary => primary_tf = Some(tf),
            PortalSlot::Secondary => secondary_tf = Some(tf),
        }
    }
    match (primary_tf, secondary_tf) {
        (Some(p), Some(s)) => vec![(p, s), (s, p)],
        _ => Vec::new(),
    }
}

/// Cast a beam ray with one level of portal refraction. Hits either world
/// geometry, the nearer portal disc (then continues from its pair), or both.
pub fn cast_beam_ray(
    spatial: &SpatialQuery,
    origin: Vec3,
    direction: Dir3,
    max_dist: f32,
    filter: &SpatialQueryFilter,
    portal_pairs: &[(Transform, Transform)],
) -> BeamHit {
    let world_hit = spatial.cast_ray(origin, direction, max_dist, true, filter);
    let world_t = world_hit.map(|h| h.distance).unwrap_or(max_dist);

    let mut nearest_portal: Option<(f32, &Transform, &Transform)> = None;
    for (entry, exit) in portal_pairs {
        let portal_normal = (entry.rotation * Vec3::Y).normalize();
        let denom = direction.dot(portal_normal);
        if denom.abs() < 1e-6 {
            continue;
        }
        let t = (entry.translation - origin).dot(portal_normal) / denom;
        if t < 0.0 || t > world_t {
            continue;
        }
        let hit_point = origin + *direction * t;
        let to_hit = hit_point - entry.translation;
        let radial = (to_hit - portal_normal * to_hit.dot(portal_normal)).length();
        if radial > PORTAL_RADIUS {
            continue;
        }
        if nearest_portal.map_or(true, |(pt, _, _)| t < pt) {
            nearest_portal = Some((t, entry, exit));
        }
    }

    let Some((portal_t, entry, exit)) = nearest_portal else {
        return BeamHit {
            primary_dist: world_t,
            impulse_info: world_hit.map(|h| (*direction, h.entity)),
            bent: None,
        };
    };

    let portal_hit_point = origin + *direction * portal_t;
    let entry_inv = entry.rotation.inverse();
    let local_hit = entry_inv * (portal_hit_point - entry.translation);
    let dir_flip = Quat::from_rotation_z(core::f32::consts::PI);

    let new_origin = exit.translation + exit.rotation * local_hit;
    let new_dir_vec = exit.rotation * dir_flip * entry_inv * *direction;

    let (bent_length, impulse_info) = match Dir3::new(new_dir_vec) {
        Ok(new_dir) => {
            let remaining = (max_dist - portal_t).max(0.0);
            let recursive = spatial.cast_ray(new_origin, new_dir, remaining, true, filter);
            let len = recursive.map(|h| h.distance).unwrap_or(remaining);
            let info = recursive.map(|h| (new_dir_vec, h.entity));
            (len, info)
        }
        Err(_) => (0.0, None),
    };

    BeamHit {
        primary_dist: portal_t,
        impulse_info,
        bent: Some(BentSegment {
            origin: new_origin,
            direction: new_dir_vec,
            length: bent_length,
        }),
    }
}

// --- HUD (per-spell, local-only) ------------------------------------------

mod hud {
    use bevy::prelude::*;

    use crate::player::LocalPlayer;
    use crate::spells::ActiveSpell;
    use crate::spells::convex_lens::LensPower;
    use crate::spells::convex_lens::MAX_LANTERN_DOTS;
    use crate::ui::hud::HudCenter;

    pub(super) fn install(app: &mut App) {
        // PostStartup so `HudCenter` (created by `ui::hud::spawn_hud` in
        // Startup) is guaranteed to exist when we attach our overlay.
        app.add_systems(PostStartup, spawn).add_systems(
            Update,
            (update_lumen_text, update_alignment_indicator),
        );
    }

    #[derive(Component)]
    struct LumenText;

    #[derive(Component)]
    struct AlignmentIndicator;

    #[derive(Component)]
    struct AlignmentDot {
        slot: usize,
    }

    const ALIGN_BOX_HALF_W: f32 = 36.0;
    const ALIGN_BOX_HALF_H: f32 = 84.0;
    const ALIGN_BORDER: f32 = 1.5;
    const ALIGN_DOT_SIZE: f32 = 6.0;
    const LUMEN_TEXT_TOP: f32 = 96.0;

    fn spawn(mut commands: Commands, center: Single<Entity, With<HudCenter>>) {
        let center = *center;
        commands.entity(center).with_children(|c| {
            // Lumen text.
            c.spawn((
                LumenText,
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(-60.0),
                    top: Val::Px(LUMEN_TEXT_TOP),
                    width: Val::Px(120.0),
                    justify_content: JustifyContent::Center,
                    display: Display::None,
                    ..default()
                },
                children![(
                    Text::new("0 lm"),
                    TextFont {
                        font_size: 16.0,
                        ..default()
                    },
                    TextColor(Color::srgba(0.7, 0.7, 0.7, 0.85)),
                )],
            ));

            // Alignment indicator box.
            let border = Color::srgba(0.85, 0.85, 0.85, 0.55);
            c.spawn((
                AlignmentIndicator,
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(-ALIGN_BOX_HALF_W),
                    top: Val::Px(-ALIGN_BOX_HALF_H),
                    width: Val::Px(ALIGN_BOX_HALF_W * 2.0),
                    height: Val::Px(ALIGN_BOX_HALF_H * 2.0),
                    display: Display::None,
                    ..default()
                },
            ))
            .with_children(|b| {
                // Top
                b.spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: Val::Px(0.0),
                        top: Val::Px(0.0),
                        width: Val::Px(ALIGN_BOX_HALF_W * 2.0),
                        height: Val::Px(ALIGN_BORDER),
                        ..default()
                    },
                    BackgroundColor(border),
                ));
                // Bottom
                b.spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: Val::Px(0.0),
                        top: Val::Px(ALIGN_BOX_HALF_H * 2.0 - ALIGN_BORDER),
                        width: Val::Px(ALIGN_BOX_HALF_W * 2.0),
                        height: Val::Px(ALIGN_BORDER),
                        ..default()
                    },
                    BackgroundColor(border),
                ));
                // Left
                b.spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: Val::Px(0.0),
                        top: Val::Px(0.0),
                        width: Val::Px(ALIGN_BORDER),
                        height: Val::Px(ALIGN_BOX_HALF_H * 2.0),
                        ..default()
                    },
                    BackgroundColor(border),
                ));
                // Right
                b.spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: Val::Px(ALIGN_BOX_HALF_W * 2.0 - ALIGN_BORDER),
                        top: Val::Px(0.0),
                        width: Val::Px(ALIGN_BORDER),
                        height: Val::Px(ALIGN_BOX_HALF_H * 2.0),
                        ..default()
                    },
                    BackgroundColor(border),
                ));
            });

            for slot in 0..MAX_LANTERN_DOTS {
                c.spawn((
                    AlignmentDot { slot },
                    Node {
                        position_type: PositionType::Absolute,
                        left: Val::Px(-ALIGN_DOT_SIZE * 0.5),
                        top: Val::Px(-ALIGN_DOT_SIZE * 0.5),
                        width: Val::Px(ALIGN_DOT_SIZE),
                        height: Val::Px(ALIGN_DOT_SIZE),
                        display: Display::None,
                        ..default()
                    },
                    BackgroundColor(Color::srgba(1.0, 0.85, 0.4, 0.9)),
                ));
            }
        });
    }

    fn update_lumen_text(
        player: Option<Single<(&LensPower, &ActiveSpell), With<LocalPlayer>>>,
        mut roots: Query<(&mut Node, &Children), With<LumenText>>,
        mut texts: Query<(&mut Text, &mut TextColor)>,
    ) {
        // Both lens-family channeled spells care about alignment: convex_lens
        // for the channel itself, iris for charging the burst from LensPower.
        let show = player
            .as_ref()
            .map(|p| p.1.0.is("convex_lens") || p.1.0.is("iris"))
            .unwrap_or(false);
        if !show {
            for (mut node, _) in &mut roots {
                node.display = Display::None;
            }
            return;
        };
        let (power, _) = *player.unwrap();
        for (mut node, children) in &mut roots {
            node.display = Display::Flex;
            for child in children.iter() {
                if let Ok((mut text, mut color)) = texts.get_mut(child) {
                    match (power.in_range, power.aligned) {
                        (false, _) => {
                            *text = Text::new("— lm");
                            color.0 = Color::srgba(0.5, 0.5, 0.5, 0.6);
                        }
                        (true, false) => {
                            *text = Text::new("misaligned");
                            color.0 = Color::srgba(1.0, 0.35, 0.35, 0.9);
                        }
                        (true, true) => {
                            let lm = power.lumens.round() as i32;
                            *text = Text::new(format!("{lm} lm"));
                            let warmth = (power.scalar / 1.0).clamp(0.0, 1.0);
                            color.0 = Color::srgba(
                                1.0,
                                0.85 + 0.15 * warmth,
                                0.55 + 0.45 * (1.0 - warmth),
                                0.85,
                            );
                        }
                    }
                }
            }
        }
    }

    fn update_alignment_indicator(
        player: Option<Single<(&LensPower, &ActiveSpell), With<LocalPlayer>>>,
        mut indicator: Query<&mut Node, (With<AlignmentIndicator>, Without<AlignmentDot>)>,
        mut dots: Query<(&AlignmentDot, &mut Node, &mut BackgroundColor)>,
    ) {
        // Both lens-family channeled spells care about alignment: convex_lens
        // for the channel itself, iris for charging the burst from LensPower.
        let show = player
            .as_ref()
            .map(|p| p.1.0.is("convex_lens") || p.1.0.is("iris"))
            .unwrap_or(false);
        if !show {
            for mut node in &mut indicator {
                node.display = Display::None;
            }
            for (_, mut node, _) in &mut dots {
                node.display = Display::None;
            }
            return;
        };
        let (power, _) = *player.unwrap();
        let show_box = if power.in_range { Display::Flex } else { Display::None };
        for mut node in &mut indicator {
            node.display = show_box;
        }

        for (dot, mut node, mut color) in &mut dots {
            let Some(source) = power.sources.get(dot.slot) else {
                node.display = Display::None;
                continue;
            };

            let yaw = source.yaw_offset.clamp(-1.6, 1.6);
            let pitch = source.pitch_offset.clamp(-1.6, 1.6);
            let dx = yaw * ALIGN_BOX_HALF_W;
            let dy = -pitch * ALIGN_BOX_HALF_H;

            node.display = Display::Flex;
            node.left = Val::Px(-ALIGN_DOT_SIZE * 0.5 + dx);
            node.top = Val::Px(-ALIGN_DOT_SIZE * 0.5 + dy);

            let is_active = power.active_lantern == Some(source.entity);
            color.0 = match (source.aligned, is_active) {
                (true, true) => Color::srgba(1.0, 0.9, 0.4, 0.95),
                (true, false) => Color::srgba(0.95, 0.85, 0.45, 0.55),
                (false, _) => Color::srgba(1.0, 0.35, 0.35, 0.9),
            };
        }
    }
}

