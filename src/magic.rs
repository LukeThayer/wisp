//! Magic v1: a single source (the lantern), a single lens (convex), and beam
//! casting whose character depends on player position relative to the lantern.
//!
//! Flow: throw lantern (Q, grenade arc) → pick up (E, when within range) →
//! hold Fire (LMB) while the Convex Lens is the active spell to project a beam
//! from the lens (the wand) along your gaze. The lantern controls power
//! (closer = stronger) and the lens-to-lantern distance vs. the lens focal
//! length determines whether the beam diverges, runs parallel, or converges to
//! a point in front of you.

use avian3d::prelude::*;
use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy_enhanced_input::prelude::*;

use crate::input::{Fire, InputMode, Pickup, ThrowLantern};
use crate::player::spells::{ActiveSpell, SpellId};
use crate::player::{Player, PlayerCamera};
use crate::spatial::{GameLayer, Portal, PortalSlot, PortalTraveler, PORTAL_RADIUS};

pub struct MagicPlugin;

impl Plugin for MagicPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Channeling>()
            .init_resource::<LensPower>()
            .init_resource::<IrisState>()
            .add_observer(on_throw_lantern)
            .add_observer(on_pickup)
            .add_observer(on_fire_start)
            .add_observer(on_fire_end)
            .add_systems(Startup, setup_beam)
            .add_systems(Update, (compute_lens_power, cast_beam).chain());
    }
}

/// Marker for the placed light source.
#[derive(Component)]
pub struct Lantern;

/// Marker for the lens position in the player's hand (child of camera).
/// Its world position is where the beam exits and its forward axis is the aim.
#[derive(Component)]
pub struct LensAnchor;

/// Marker for the beam visualization entity (procedural frustum mesh).
#[derive(Component)]
pub struct Beam;

/// Marker for the secondary beam segment that continues out of a portal
/// when the primary ray refracts through one. Hidden when the beam doesn't
/// touch a portal.
#[derive(Component)]
pub struct PortalBeam;

/// True while the player is holding the Fire action.
#[derive(Resource, Default)]
pub struct Channeling(pub bool);

/// Live state for the Iris lens — variable-aperture charge-and-release.
///
/// While Fire is held and at least one source is feeding the lens, `charge`
/// rises. On release the accumulated value is dumped as a single high-impulse
/// burst; `burst_timer` keeps the beam visible for the brief flash.
#[derive(Resource, Default)]
pub struct IrisState {
    /// Current accumulated charge, 0..IRIS_MAX_CHARGE.
    pub charge: f32,
    /// Charge value captured at the moment of release; used to scale the burst.
    pub burst_charge: f32,
    /// Seconds remaining of the visible burst.
    pub burst_timer: f32,
}

/// Shared truth for "how much power is entering the lens right now."
/// Computed once per frame by [`compute_lens_power`]; read by `cast_beam` and the HUD.
#[derive(Resource, Default)]
pub struct LensPower {
    /// 0.0–1.0 scalar derived from lens-to-lantern distance.
    pub scalar: f32,
    /// True iff there's a placed lantern within tether range.
    pub in_range: bool,
    /// True iff the lens's principal axis is within the alignment cone of the
    /// lantern→lens radial direction (i.e., the lens is facing away from the
    /// lantern, not edge-on or backwards).
    pub aligned: bool,
    /// True iff in_range AND aligned AND channeling AND active spell is ConvexLens.
    pub lens_active: bool,
    /// Distance from the lens to the lantern, or 0 if not in range.
    pub lens_distance: f32,
    /// Display value in arbitrary "lumens" units. Equals the sum of every
    /// aligned source's individual contribution; can exceed MAX_LUMENS when
    /// multiple sources stack.
    pub lumens: f32,
    /// The placed lantern currently driving the lens — the one closest to
    /// alignment among all in-range placements. None if none are in range.
    pub active_lantern: Option<Entity>,
    /// All placed lanterns within tether range. Used by the HUD to render
    /// one alignment dot per source so the player can see every available
    /// option at once.
    pub sources: Vec<LensSource>,
}

/// Per-lantern info exposed for the HUD's alignment indicator.
#[derive(Clone, Copy)]
pub struct LensSource {
    pub entity: Entity,
    /// Normalized signed offset, ±1.0 = at the yaw tolerance boundary.
    pub yaw_offset: f32,
    /// Normalized signed offset, ±1.0 = at the pitch tolerance boundary.
    pub pitch_offset: f32,
    /// True iff this source is within tolerance.
    pub aligned: bool,
    /// Distance from the lens to this lantern.
    pub lens_distance: f32,
}

// --- Tuning ----------------------------------------------------------------

/// Maximum number of placed lanterns at once.
pub const MAX_LANTERNS: usize = 3;

const LANTERN_RADIUS: f32 = 0.12;
const LANTERN_MASS: f32 = 0.4;
const LANTERN_THROW_FORWARD: f32 = 9.0;
const LANTERN_THROW_UP: f32 = 3.0;
const LANTERN_SPAWN_OFFSET_FORWARD: f32 = 0.6;

const PICKUP_RADIUS: f32 = 1.8;

const BEAM_MAX_RANGE: f32 = 30.0;
/// Peak impulse magnitude (kg·m/s) per second of channel at point-blank.
const BEAM_IMPULSE_RATE: f32 = 80.0;
/// Distance from lantern at which beam power has decayed to zero.
const BEAM_POWER_FALLOFF: f32 = 14.0;
/// Past this distance from lantern, the channel breaks.
const TETHER_MAX_RANGE: f32 = 25.0;

/// Physical radius of the convex lens, in metres.
const LENS_RADIUS: f32 = 0.10;
/// Focal length of the lens, in metres. Stand at this distance from the
/// lantern to get a parallel beam.
const FOCAL_LENGTH: f32 = 3.0;
/// Mesh tessellation around the beam's circular cross-section.
const BEAM_SEGMENTS: u32 = 24;
/// Display scale: scalar 1.0 → MAX_LUMENS lumens shown on HUD.
pub const MAX_LUMENS: f32 = 1200.0;
/// Asymmetric alignment tolerance for the lens's principal axis relative to
/// the lantern→lens radial. More forgiving up/down (vertical pitch difference)
/// than left/right (horizontal yaw difference) — easier to keep the beam alive
/// while you look up or down at a target.
const LENS_ALIGNMENT_YAW_DEG: f32 = 15.0;
const LENS_ALIGNMENT_PITCH_DEG: f32 = 35.0;

// --- Iris tuning -----------------------------------------------------------
/// Charge gained per second of channeling, multiplied by `LensPower::scalar`.
const IRIS_CHARGE_RATE: f32 = 1.0;
/// Charge lost per second when not actively fed (released or misaligned).
const IRIS_DECAY_RATE: f32 = 2.0;
/// Cap on accumulated charge. Matches the maximum sum of three aligned sources.
pub const IRIS_MAX_CHARGE: f32 = 3.0;
/// Visible duration of the burst flash, in seconds.
const IRIS_BURST_DURATION: f32 = 0.12;
/// Impulse delivered per unit of charge.
const IRIS_BURST_IMPULSE: f32 = 150.0;
/// Below this charge level, releasing Fire is a no-op (avoids fizzles).
const IRIS_BURST_MIN: f32 = 0.05;
/// Radius of the burst beam — narrower than a convex cone (it's a focused dump).
const IRIS_BURST_RADIUS: f32 = 0.07;

// --- Lantern placement -----------------------------------------------------

pub fn on_throw_lantern(
    _: On<Start<ThrowLantern>>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    existing: Query<Entity, With<Lantern>>,
    cam: Single<&GlobalTransform, With<PlayerCamera>>,
) {
    if existing.iter().count() >= MAX_LANTERNS {
        return;
    }

    let cam_pos = cam.translation();
    let cam_rot = cam.rotation();
    let forward = cam_rot * Vec3::NEG_Z;

    let spawn_pos = cam_pos + forward * LANTERN_SPAWN_OFFSET_FORWARD;
    let velocity = forward * LANTERN_THROW_FORWARD + Vec3::Y * LANTERN_THROW_UP;

    commands.spawn((
        Name::new("Lantern"),
        Lantern,
        PortalTraveler,
        Mesh3d(meshes.add(Sphere::new(LANTERN_RADIUS))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 0.95, 0.7),
            emissive: LinearRgba::rgb(8.0, 6.5, 2.5),
            ..default()
        })),
        Transform::from_translation(spawn_pos),
        (
            RigidBody::Dynamic,
            Collider::sphere(LANTERN_RADIUS),
            Mass(LANTERN_MASS),
            LinearVelocity(velocity),
            Friction::new(1.0),
            LinearDamping(0.4),
            AngularDamping(0.5),
            Restitution::new(0.3),
            CollisionLayers::new(GameLayer::Default, LayerMask::ALL),
        ),
        children![(
            Name::new("LanternLight"),
            PointLight {
                color: Color::srgb(1.0, 0.92, 0.65),
                intensity: 60_000.0,
                range: 16.0,
                shadows_enabled: false,
                ..default()
            },
            Transform::default(),
        )],
    ));
}

pub fn on_pickup(
    _: On<Start<Pickup>>,
    mut commands: Commands,
    lanterns: Query<(Entity, &Transform), With<Lantern>>,
    player: Single<&Transform, With<Player>>,
) {
    for (entity, lantern_tf) in &lanterns {
        if player.translation.distance(lantern_tf.translation) <= PICKUP_RADIUS {
            commands.entity(entity).despawn();
        }
    }
}

// --- Channeling state ------------------------------------------------------

pub fn on_fire_start(_: On<Start<Fire>>, mut channeling: ResMut<Channeling>) {
    channeling.0 = true;
}

pub fn on_fire_end(
    _: On<Complete<Fire>>,
    mut channeling: ResMut<Channeling>,
    mut iris: ResMut<IrisState>,
    active_spell: Res<ActiveSpell>,
    spatial: SpatialQuery,
    lens: Single<&GlobalTransform, With<LensAnchor>>,
    player_entity: Single<Entity, With<Player>>,
    lanterns: Query<(Entity, &GlobalTransform), With<Lantern>>,
    portals: Query<(&Portal, &GlobalTransform)>,
    mut forces: Query<Forces>,
) {
    channeling.0 = false;

    if active_spell.0 != SpellId::Iris {
        return;
    }
    if iris.charge < IRIS_BURST_MIN {
        // Released too early to matter.
        iris.charge = 0.0;
        return;
    }

    // Capture the accumulated charge; the visual burst rides on `burst_timer`
    // while `cast_beam` draws the brief flash.
    iris.burst_charge = iris.charge;
    iris.burst_timer = IRIS_BURST_DURATION;
    iris.charge = 0.0;

    // One raycast, one big impulse — with portal refraction so the burst can
    // hit anything visible through the portals as well.
    let lens_pos = lens.translation();
    let beam_dir = lens.compute_transform().forward();
    let excluded: Vec<Entity> = lanterns
        .iter()
        .map(|(e, _)| e)
        .chain(std::iter::once(*player_entity))
        .collect();
    let filter = SpatialQueryFilter::from_excluded_entities(excluded);

    let mut primary_tf: Option<Transform> = None;
    let mut secondary_tf: Option<Transform> = None;
    for (portal, gt) in &portals {
        let tf = gt.compute_transform();
        match portal.slot {
            PortalSlot::Primary => primary_tf = Some(tf),
            PortalSlot::Secondary => secondary_tf = Some(tf),
        }
    }
    let portal_pairs: Vec<(Transform, Transform)> = match (primary_tf, secondary_tf) {
        (Some(p), Some(s)) => vec![(p, s), (s, p)],
        _ => Vec::new(),
    };

    let hit = cast_beam_ray(
        &spatial,
        lens_pos,
        beam_dir,
        BEAM_MAX_RANGE,
        &filter,
        &portal_pairs,
    );
    if let Some((dir_at_hit, target)) = hit.impulse_info {
        if let Ok(mut f) = forces.get_mut(target) {
            let impulse = dir_at_hit * IRIS_BURST_IMPULSE * iris.burst_charge;
            f.apply_linear_impulse(impulse);
        }
    }
}

// --- Beam visualization ----------------------------------------------------

fn setup_beam(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // Initial placeholder geometry; rebuilt every frame while channeling.
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

    // Continuation segment, drawn when the beam refracts through a portal.
    // Shares the same emissive material; its own mesh is rewritten each frame.
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

/// Builds a frustum (truncated cone) extending from z=0 (radius r_start) to
/// z=-length (radius r_end). Oriented in -Z so it lines up with Bevy's "forward."
fn build_frustum_mesh(r_start: f32, r_end: f32, length: f32, segments: u32) -> Mesh {
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
        // Radial normal — good enough for a glowing translucent beam.
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

/// In-place vertex rewrite — keeps the same index buffer (segment count fixed).
fn rewrite_frustum(mesh: &mut Mesh, r_start: f32, r_end: f32, length: f32, segments: u32) {
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

// --- Lens power (shared truth) ---------------------------------------------

pub fn compute_lens_power(
    mut power: ResMut<LensPower>,
    channeling: Res<Channeling>,
    active_spell: Res<ActiveSpell>,
    lens: Single<&GlobalTransform, With<LensAnchor>>,
    lanterns: Query<(Entity, &GlobalTransform), With<Lantern>>,
) {
    let lens_pos = lens.translation();
    let lens_rot_inv = lens.rotation().inverse();
    let yaw_tol = LENS_ALIGNMENT_YAW_DEG.to_radians();
    let pitch_tol = LENS_ALIGNMENT_PITCH_DEG.to_radians();

    // Score every in-range lantern. The HUD shows them all; the beam picks one.
    let mut sources: Vec<LensSource> = Vec::with_capacity(MAX_LANTERNS);
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

    // Pick the active source: prefer aligned, then closer.
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
        return;
    };
    let active = sources[active_idx];

    // Total power = sum of contributions from every aligned source. With three
    // aligned lanterns at point-blank you can reach 3.0; the beam impulse and
    // the lumen readout both scale with this combined value.
    let scalar: f32 = sources
        .iter()
        .filter(|s| s.aligned)
        .map(|s| (1.0 - s.lens_distance / BEAM_POWER_FALLOFF).clamp(0.0, 1.0))
        .sum();
    // Either lens spell can claim the lens; cast_beam dispatches by id.
    let is_lens_spell = matches!(active_spell.0, SpellId::ConvexLens | SpellId::Iris);
    let lens_active = scalar > 0.0 && channeling.0 && is_lens_spell;
    let lumens = scalar * MAX_LUMENS;

    *power = LensPower {
        scalar,
        in_range: true,
        aligned: active.aligned,
        lens_active,
        lens_distance: active.lens_distance,
        lumens,
        active_lantern: Some(active.entity),
        sources,
    };
}

// --- Beam cast -------------------------------------------------------------

pub fn cast_beam(
    time: Res<Time>,
    mode: Res<InputMode>,
    power: Res<LensPower>,
    active_spell: Res<ActiveSpell>,
    channeling: Res<Channeling>,
    mut iris: ResMut<IrisState>,
    spatial: SpatialQuery,
    mut gizmos: Gizmos,
    mut meshes: ResMut<Assets<Mesh>>,
    lens: Single<&GlobalTransform, With<LensAnchor>>,
    player_entity: Single<Entity, With<Player>>,
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
) {
    // Build portal pairs once for use by the beam-ray helper. Both directions
    // are included so the ray can hit either disc.
    let mut primary_tf: Option<Transform> = None;
    let mut secondary_tf: Option<Transform> = None;
    for (portal, gt) in &portals {
        let tf = gt.compute_transform();
        match portal.slot {
            PortalSlot::Primary => primary_tf = Some(tf),
            PortalSlot::Secondary => secondary_tf = Some(tf),
        }
    }
    let portal_pairs: Vec<(Transform, Transform)> = match (primary_tf, secondary_tf) {
        (Some(p), Some(s)) => vec![(p, s), (s, p)],
        _ => Vec::new(),
    };
    let (beam_tf, beam_vis, beam_mesh) = &mut *beam;
    let (portal_beam_tf, portal_beam_vis, portal_beam_mesh) = &mut *portal_beam;

    // Drop iris state when not on its lens, so a stale charge can't bleed
    // into the next selected spell.
    if active_spell.0 != SpellId::Iris {
        iris.charge = 0.0;
        iris.burst_timer = 0.0;
    }

    if *mode != InputMode::Player {
        **beam_vis = Visibility::Hidden;
        **portal_beam_vis = Visibility::Hidden;
        return;
    }

    let lens_pos = lens.translation();
    let beam_dir = lens.compute_transform().forward();
    let dt = time.delta_secs();

    match active_spell.0 {
        SpellId::ConvexLens => {
            if !power.lens_active {
                **beam_vis = Visibility::Hidden;
                **portal_beam_vis = Visibility::Hidden;
                return;
            }

            // Tether from every aligned source.
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
                .chain(std::iter::once(*player_entity))
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

            // Portal continuation: draw a uniform-radius cylinder from the
            // exit disc along the bent ray direction up to the next hit.
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

            if let Some((dir_at_hit, target)) = hit.impulse_info {
                if let Ok(mut f) = forces.get_mut(target) {
                    let impulse =
                        dir_at_hit * BEAM_IMPULSE_RATE * power.scalar * dt;
                    f.apply_linear_impulse(impulse);
                }
            }
        }
        SpellId::Iris => {
            if iris.burst_timer > 0.0 {
                // --- Burst flash ---
                iris.burst_timer = (iris.burst_timer - dt).max(0.0);

                for source in &power.sources {
                    if !source.aligned {
                        continue;
                    }
                    if let Ok((_, tf)) = lanterns.get(source.entity) {
                        gizmos.line(
                            tf.translation(),
                            lens_pos,
                            Color::srgb(1.0, 0.85, 0.4),
                        );
                    }
                }

                let excluded: Vec<Entity> = lanterns
                    .iter()
                    .map(|(e, _)| e)
                    .chain(std::iter::once(*player_entity))
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
                        portal_beam_tf.rotation =
                            Quat::from_rotation_arc(Vec3::NEG_Z, *bent_dir);
                        **portal_beam_vis = Visibility::Visible;
                    } else {
                        **portal_beam_vis = Visibility::Hidden;
                    }
                } else {
                    **portal_beam_vis = Visibility::Hidden;
                }
            } else {
                // --- Charging / decaying ---
                **beam_vis = Visibility::Hidden;
                **portal_beam_vis = Visibility::Hidden;
                if channeling.0 && power.scalar > 0.0 {
                    iris.charge = (iris.charge
                        + power.scalar * IRIS_CHARGE_RATE * dt)
                        .min(IRIS_MAX_CHARGE);
                } else if iris.charge > 0.0 {
                    iris.charge = (iris.charge - IRIS_DECAY_RATE * dt).max(0.0);
                }
            }
        }
        _ => {
            **beam_vis = Visibility::Hidden;
            **portal_beam_vis = Visibility::Hidden;
        }
    }
}

/// Output of a portal-aware beam raycast.
pub struct BeamHit {
    /// How far along the original ray to draw the primary segment — either
    /// to the world geometry it hit, or to the portal disc it entered.
    pub primary_dist: f32,
    /// `(direction_at_final_hit, entity)` for applying impulse. When the
    /// ray refracted through a portal, the direction is the post-bend ray.
    pub impulse_info: Option<(Vec3, Entity)>,
    /// Present when the ray hit a portal disc — the continuation of the
    /// beam emerging from the paired portal.
    pub bent: Option<BentSegment>,
}

/// A beam continuation that emerged from a portal pair's exit.
pub struct BentSegment {
    pub origin: Vec3,
    pub direction: Vec3,
    pub length: f32,
}

/// Cast a beam ray with one level of portal refraction. See [`BeamHit`].
///
/// Position math matches the object-traveler convention (no `pos_flip`).
/// Direction math applies an X-180 flip in entry-local — so "into entry"
/// becomes "out of exit" along the portal normal, the same way a thrown
/// object's velocity is reversed across the portal frame.
fn cast_beam_ray(
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
    // Position is a pure frame change. Direction gets a Z-180 flip in the
    // local frame: still flips the normal axis (so "into entry" becomes
    // "out of exit") but leaves the in-plane axes oriented such that the
    // player's left/right and up/down aim shifts come out the same way on
    // the bent beam. X-180 would have inverted both.
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
