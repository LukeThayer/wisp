//! Portal spell: paired portals (Primary on LMB, Secondary on RMB). Each
//! cast raycasts forward and either sticks the portal to the hit surface or
//! floats it in the air at a fixed distance. Walking into either portal
//! teleports the player to the other; physical props with `PortalTraveler`
//! travel through the pair too.
//!
//! Per-player components: `PrevPos` (last frame's body translation) and
//! `PortalLockout` (a small set of portal entities the player just used).
//! `PortalTraveler` entities get their own `PrevPos` / `PortalLockout`.

use std::collections::HashSet;

use avian3d::prelude::*;
use bevy::camera::visibility::RenderLayers;
use bevy::camera::RenderTarget;
use bevy::pbr::{Material, MaterialPlugin};
use bevy::prelude::*;
use bevy::render::render_resource::{AsBindGroup, TextureFormat};
use bevy::shader::ShaderRef;
use bevy::window::PrimaryWindow;
use lightyear::prelude::{MessageSender, Replicated};

use crate::net::protocol::{NetworkedPortal, PlacePortalMessage, PlayerInputChannel};
use crate::physics::GameLayer;
use crate::player::{Facing, LocalPlayer, Player, PlayerCamera, SELF_BODY_LAYER};
use crate::spells::data::HandlerId;
use crate::spells::handlers::HandlerRegistry;
use crate::spells::markers::Lantern;

pub fn register(app: &mut App) {
    // Catalog supplies the radial-menu label via `assets/spells/portal.spell.ron`.
    // Placement runs as a cast-engine handler so the wind-up `ChargingDef`
    // gates correctly — the engine only dispatches the handler when the
    // cast reaches Releasing. The through-portal teleport pipeline stays
    // as plain `Update` systems since it needs to run regardless of cast
    // state.
    let mut registry = app.world_mut().resource_mut::<HandlerRegistry>();
    registry.register(
        HandlerId("portal.place_primary".to_string()),
        place_primary_handler,
    );
    registry.register(
        HandlerId("portal.place_secondary".to_string()),
        place_secondary_handler,
    );
    app.add_plugins(PortalPlugin);
}

struct PortalPlugin;

impl Plugin for PortalPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(MaterialPlugin::<PortalMaterial>::default())
            .add_observer(on_networked_portal_replicated)
            .add_systems(
                Update,
                (
                    update_player_falling,
                    update_traveler_falling,
                    // Stage Q.5b: `process_teleports` (player) and
                    // `process_traveler_teleports` (props/lanterns) are
                    // RETIRED on the client. The server's
                    // `server_portal_teleport` is the sole authority
                    // for teleporting bodies. Running them locally was
                    // causing rapid A↔B oscillation: the local rig's
                    // Position is overwritten each frame by
                    // `sync_local_player_from_server`, which makes
                    // prev_pos and curr_pos appear to "cross" portals
                    // synthetically — local kept retriggering teleport
                    // against the wrong portal. UX trade-off: camera no
                    // longer auto-rotates through the portal; the
                    // player exits facing whatever direction they were
                    // facing when they entered. Fixable later by either
                    // replicating a "post-teleport facing snap" event
                    // or running a *facing-only* local detector that
                    // doesn't touch position.
                    update_portal_cameras,
                    cleanup_orphan_portal_cameras,
                )
                    .chain(),
            );
    }
}

// --- Public types ---------------------------------------------------------

#[derive(Asset, TypePath, AsBindGroup, Clone)]
pub struct PortalMaterial {
    #[uniform(0)]
    pub rim_color: LinearRgba,
    #[texture(1)]
    #[sampler(2)]
    pub portal_tex: Handle<Image>,
}

impl Material for PortalMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/portal.wgsl".into()
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum PortalSlot {
    Primary,
    Secondary,
}

#[derive(Component)]
pub struct Portal {
    pub slot: PortalSlot,
}

/// Marker on the off-screen Camera3d that renders the through-portal view
/// for `portal`. Despawned when its portal goes away.
#[derive(Component)]
pub struct PortalCamera {
    pub portal: Entity,
}

/// Any dynamic entity that should be carried through portals (props,
/// lanterns, etc.). The player has its own dedicated teleport path because
/// of body/camera split and Facing.
#[derive(Component)]
pub struct PortalTraveler;

/// Previous-frame translation, used to detect sign-flip crossings against
/// portal planes. Lives on Player and on each PortalTraveler.
#[derive(Component, Default)]
pub struct PrevPos(pub Option<Vec3>);

/// Portals this entity just teleported through and can't re-trigger until it
/// walks away from them. Per-player so each client manages its own bounce
/// protection; per-traveler so each object does the same.
#[derive(Component, Default)]
pub struct PortalLockout(pub HashSet<Entity>);

// --- Tuning ---------------------------------------------------------------

pub const PORTAL_RADIUS: f32 = 1.4;
const PORTAL_THICKNESS: f32 = 0.04;
const AIR_PORTAL_DISTANCE: f32 = 2.5;
const PORTAL_RAYCAST_RANGE: f32 = 15.0;
const HORIZONTAL_NORMAL_DOT_Y: f32 = 0.7;
const HORIZONTAL_DISC_DEPTH: f32 = 2.0;
// Stage Q.5b retired the local-only teleport systems (see the
// `process_teleports` / `process_traveler_teleports` comment further
// down). These constants belong to those systems; we keep them so
// re-enabling either system is a one-line revert.
#[allow(dead_code)]
const PORTAL_LOCKOUT_RADIUS: f32 = PORTAL_RADIUS * 1.5;
#[allow(dead_code)]
const PLAYER_CAPSULE_RADIUS: f32 = 0.4;
#[allow(dead_code)]
const PLAYER_CAPSULE_HALF_HEIGHT: f32 = 0.6;
#[allow(dead_code)]
const CAMERA_OFFSET: Vec3 = Vec3::new(0.0, 0.7, 0.0);
const SURFACE_INSET: f32 = 0.02;
const PORTAL_TEXTURE_MAX_DIM: u32 = 1280;
const PORTAL_CAMERA_FOV_DEG: f32 = 90.0;

fn portal_texture_size(window: &Window) -> (u32, u32) {
    let w = window.physical_width().max(1);
    let h = window.physical_height().max(1);
    let longest = w.max(h);
    if longest <= PORTAL_TEXTURE_MAX_DIM {
        (w, h)
    } else {
        let scale = PORTAL_TEXTURE_MAX_DIM as f32 / longest as f32;
        (
            ((w as f32) * scale).round().max(1.0) as u32,
            ((h as f32) * scale).round().max(1.0) as u32,
        )
    }
}

// --- Placement ------------------------------------------------------------
//
// Placement is server-authoritative. Each client raycasts locally (every
// peer has the same static world geometry), sends a `PlacePortalMessage`,
// and the server spawns / despawns a replicated `NetworkedPortal`. The
// local visual (mesh, render-to-texture camera, PortalMaterial) is
// attached client-side by an observer on `Add<NetworkedPortal>` (see
// `net::replication`).

fn place_primary_handler(world: &mut World, _ctx: &crate::spells::handlers::CastContext) {
    place_portal_handler(world, PortalSlot::Primary);
}

fn place_secondary_handler(world: &mut World, _ctx: &crate::spells::handlers::CastContext) {
    place_portal_handler(world, PortalSlot::Secondary);
}

fn place_portal_handler(world: &mut World, slot: PortalSlot) {
    use bevy::ecs::system::SystemState;
    let mut sys_state: SystemState<(
        SpatialQuery,
        Single<&GlobalTransform, (With<PlayerCamera>, With<LocalPlayer>)>,
        Single<Entity, (With<Player>, With<LocalPlayer>)>,
        Query<Entity, With<Portal>>,
        Query<Entity, With<Lantern>>,
        Option<Single<&mut MessageSender<PlacePortalMessage>>>,
    )> = SystemState::new(world);
    let (spatial, cam, player_entity, portals, lanterns, sender) =
        sys_state.get_mut(world);
    let Some(mut sender) = sender else { return };
    let cam_pos = cam.translation();
    let forward = cam.rotation() * Vec3::NEG_Z;

    let (pos, normal) = if let Ok(dir) = Dir3::new(forward) {
        let excluded: Vec<Entity> = lanterns
            .iter()
            .chain(portals.iter())
            .chain(std::iter::once(*player_entity))
            .collect();
        let filter = SpatialQueryFilter::from_excluded_entities(excluded);
        match spatial.cast_ray(cam_pos, dir, PORTAL_RAYCAST_RANGE, true, &filter) {
            Some(hit) => (
                cam_pos + forward * hit.distance + hit.normal * SURFACE_INSET,
                hit.normal,
            ),
            None => (cam_pos + forward * AIR_PORTAL_DISTANCE, -forward),
        }
    } else {
        return;
    };

    let _ = sender.send::<PlayerInputChannel>(PlacePortalMessage {
        slot,
        position: [pos.x, pos.y, pos.z],
        normal: [normal.x, normal.y, normal.z],
    });
}

/// Attach local visuals (mesh, PortalMaterial, render-to-texture camera) +
/// the local `Portal { slot }` marker when a server-spawned
/// `NetworkedPortal` arrives. Rotation is derived locally from `normal`;
/// translation is driven each frame by `NetworkedPosition`.
fn on_networked_portal_replicated(
    trigger: On<Add, NetworkedPortal>,
    replicated: Query<&NetworkedPortal, With<Replicated>>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut portal_materials: ResMut<Assets<PortalMaterial>>,
    mut images: ResMut<Assets<Image>>,
    window: Single<&Window, With<PrimaryWindow>>,
) {
    let Ok(np) = replicated.get(trigger.entity) else {
        return;
    };
    let slot = np.slot;
    let normal = Vec3::from(np.normal);
    let rotation = disc_rotation(normal);
    let (rim_color, name) = match slot {
        PortalSlot::Primary => (LinearRgba::rgb(1.5, 0.55, 0.12), "Portal-Primary"),
        PortalSlot::Secondary => (LinearRgba::rgb(0.25, 0.85, 2.2), "Portal-Secondary"),
    };

    let (tex_w, tex_h) = portal_texture_size(&window);
    let render_image = images.add(Image::new_target_texture(
        tex_w,
        tex_h,
        TextureFormat::Bgra8UnormSrgb,
        None,
    ));

    let portal_entity = trigger.entity;
    commands.entity(portal_entity).insert((
        Name::new(name),
        Portal { slot },
        Mesh3d(meshes.add(Cylinder::new(PORTAL_RADIUS, PORTAL_THICKNESS))),
        MeshMaterial3d(portal_materials.add(PortalMaterial {
            rim_color,
            portal_tex: render_image.clone(),
        })),
        Visibility::default(),
        Transform::default().with_rotation(rotation),
    ));

    commands.spawn((
        Name::new(format!("{name}-Camera")),
        PortalCamera {
            portal: portal_entity,
        },
        Camera3d::default(),
        Camera {
            order: -1,
            is_active: false,
            ..default()
        },
        RenderTarget::Image(render_image.into()),
        Projection::Perspective(PerspectiveProjection {
            fov: PORTAL_CAMERA_FOV_DEG.to_radians(),
            ..default()
        }),
        Transform::default(),
        // Include the local player's body layer so a portal pair lets
        // the user see their own character from outside. Default world
        // (layer 0) still renders. Other clients' bodies are on layer 0
        // already so they're unaffected.
        RenderLayers::from_layers(&[0, SELF_BODY_LAYER]),
    ));
}

// --- Through-portal rendering --------------------------------------------

pub fn disc_rotation(normal: Vec3) -> Quat {
    let y_axis = normal.normalize_or_zero();
    if y_axis == Vec3::ZERO {
        return Quat::IDENTITY;
    }

    let world_up = Vec3::Y;
    let z_axis = if y_axis.cross(world_up).length_squared() < 1e-6 {
        Vec3::Z
    } else {
        let projected = world_up - y_axis * y_axis.dot(world_up);
        projected.normalize()
    };
    let x_axis = y_axis.cross(z_axis);

    Quat::from_mat3(&Mat3::from_cols(x_axis, y_axis, z_axis))
}

fn portal_virtual_transform(
    player_tf: Transform,
    entry: &Transform,
    exit: &Transform,
) -> Transform {
    let pos_flip = Quat::from_rotation_y(core::f32::consts::PI);
    let rot_flip = Quat::from_rotation_z(core::f32::consts::PI);

    let entry_inv_rot = entry.rotation.inverse();
    let local_pos = entry_inv_rot * (player_tf.translation - entry.translation);
    let local_forward = rot_flip * (entry_inv_rot * (player_tf.rotation * Vec3::NEG_Z));

    let virtual_pos = exit.translation + exit.rotation * (pos_flip * local_pos);
    let forward = exit.rotation * local_forward;

    let world_up = Vec3::Y;
    let up_ref = if forward.cross(world_up).length_squared() > 1e-6 {
        world_up
    } else {
        let player_fwd = player_tf.rotation * Vec3::NEG_Z;
        Vec3::new(player_fwd.x, 0.0, player_fwd.z)
            .try_normalize()
            .unwrap_or(Vec3::X)
    };

    let mut tf = Transform::from_translation(virtual_pos);
    tf.look_to(forward, up_ref);
    tf
}

fn update_portal_cameras(
    player_cam: Single<&GlobalTransform, (With<PlayerCamera>, With<LocalPlayer>)>,
    portals: Query<(Entity, &Portal, &GlobalTransform)>,
    mut cameras: Query<(&PortalCamera, &mut Transform, &mut Camera), Without<Portal>>,
) {
    let mut primary: Option<(Entity, Transform)> = None;
    let mut secondary: Option<(Entity, Transform)> = None;
    for (e, portal, gt) in &portals {
        match portal.slot {
            PortalSlot::Primary => primary = Some((e, gt.compute_transform())),
            PortalSlot::Secondary => secondary = Some((e, gt.compute_transform())),
        }
    }

    let pair_ready = primary.is_some() && secondary.is_some();
    let player_tf = player_cam.compute_transform();

    for (portal_cam, mut cam_tf, mut camera) in &mut cameras {
        if !pair_ready {
            if camera.is_active {
                camera.is_active = false;
            }
            continue;
        }

        let (primary_data, secondary_data) =
            (primary.as_ref().unwrap(), secondary.as_ref().unwrap());
        let (entry, exit) = if portal_cam.portal == primary_data.0 {
            (&primary_data.1, &secondary_data.1)
        } else if portal_cam.portal == secondary_data.0 {
            (&secondary_data.1, &primary_data.1)
        } else {
            continue;
        };

        *cam_tf = portal_virtual_transform(player_tf, entry, exit);
        if !camera.is_active {
            camera.is_active = true;
        }
    }
}

fn cleanup_orphan_portal_cameras(
    mut commands: Commands,
    cameras: Query<(Entity, &PortalCamera)>,
    portals: Query<(), With<Portal>>,
) {
    for (cam_entity, portal_cam) in &cameras {
        if portals.get(portal_cam.portal).is_err() {
            commands.entity(cam_entity).despawn();
        }
    }
}

// --- Falling-through-floor support ---------------------------------------

fn update_player_falling(
    mut commands: Commands,
    portals: Query<(Entity, &Portal, &GlobalTransform)>,
    mut player: Query<
        (Entity, &Transform, &CollisionLayers, &PortalLockout),
        With<Player>,
    >,
) {
    for (player_entity, player_tf, layers, lockout) in &mut player {
        let mut in_threshold = false;
        for (e, _portal, gt) in &portals {
            if lockout.0.contains(&e) {
                continue;
            }
            let portal_tf = gt.compute_transform();
            let normal = (portal_tf.rotation * Vec3::Y).normalize();
            if normal.y.abs() < HORIZONTAL_NORMAL_DOT_Y {
                continue;
            }
            let rel = player_tf.translation - portal_tf.translation;
            let along = rel.dot(normal);
            let radial = (rel - normal * along).length();
            if radial < PORTAL_RADIUS && along.abs() < HORIZONTAL_DISC_DEPTH {
                in_threshold = true;
                break;
            }
        }

        let target = if in_threshold {
            CollisionLayers::new(GameLayer::Player, [GameLayer::Default, GameLayer::Player])
        } else {
            CollisionLayers::new(GameLayer::Player, LayerMask::ALL)
        };

        if *layers != target {
            commands.entity(player_entity).insert(target);
        }
    }
}

fn update_traveler_falling(
    mut commands: Commands,
    portals: Query<(Entity, &Portal, &GlobalTransform)>,
    mut travelers: Query<
        (Entity, &Transform, &CollisionLayers, &PortalLockout),
        (With<PortalTraveler>, Without<Player>),
    >,
) {
    for (traveler_entity, traveler_tf, layers, lockout) in &mut travelers {
        let mut in_threshold = false;
        for (e, _portal, gt) in &portals {
            if lockout.0.contains(&e) {
                continue;
            }
            let portal_tf = gt.compute_transform();
            let normal = (portal_tf.rotation * Vec3::Y).normalize();
            if normal.y.abs() < HORIZONTAL_NORMAL_DOT_Y {
                continue;
            }
            let rel = traveler_tf.translation - portal_tf.translation;
            let along = rel.dot(normal);
            let radial = (rel - normal * along).length();
            if radial < PORTAL_RADIUS && along.abs() < HORIZONTAL_DISC_DEPTH {
                in_threshold = true;
                break;
            }
        }

        let target = if in_threshold {
            CollisionLayers::new(GameLayer::Default, [GameLayer::Default, GameLayer::Player])
        } else {
            CollisionLayers::new(GameLayer::Default, LayerMask::ALL)
        };

        if *layers != target {
            commands.entity(traveler_entity).insert(target);
        }

        if in_threshold {
            commands.entity(traveler_entity).remove::<Sleeping>();
        }
    }
}

// --- Teleport: travelers (props, lanterns) -------------------------------

// Retired client-side (Stage Q.5b): the server runs
// `server_portal_teleport` authoritatively. Kept here so the local-only
// path can be re-enabled if we want a "facing-only" detector later
// (see the IrisPlugin scheduling comment above).
#[allow(dead_code)]
fn process_traveler_teleports(
    portals: Query<(Entity, &Portal, &Transform)>,
    mut travelers: Query<
        (
            Entity,
            &mut Transform,
            &mut Position,
            &mut LinearVelocity,
            &mut PrevPos,
            &mut PortalLockout,
        ),
        (With<PortalTraveler>, Without<Portal>, Without<Player>),
    >,
) {
    let mut primary: Option<(Entity, Transform)> = None;
    let mut secondary: Option<(Entity, Transform)> = None;
    for (e, portal, tf) in &portals {
        match portal.slot {
            PortalSlot::Primary => primary = Some((e, *tf)),
            PortalSlot::Secondary => secondary = Some((e, *tf)),
        }
    }
    let pair = match (primary, secondary) {
        (Some(p), Some(s)) => Some((p, s)),
        _ => None,
    };

    // Always update prev_pos at the end of the loop, even when there's no
    // pair yet — otherwise the first frame after both portals exist would
    // miss its sign-flip because prev_pos was never set.
    for (_entity, mut tf, mut avian_pos, mut velocity, mut prev, mut lockout) in
        &mut travelers
    {
        let pos = tf.translation;

        // Clear lockouts for portals this traveler has moved away from.
        lockout.0.retain(|portal_e| match portals.get(*portal_e) {
            Ok((_, _, ptf)) => (pos - ptf.translation).length() <= PORTAL_LOCKOUT_RADIUS,
            Err(_) => false,
        });

        let Some(((primary_e, primary_tf), (secondary_e, secondary_tf))) = pair else {
            prev.0 = Some(pos);
            continue;
        };

        let pairs = [
            (primary_e, secondary_e, &primary_tf, &secondary_tf),
            (secondary_e, primary_e, &secondary_tf, &primary_tf),
        ];

        let prev_pos = prev.0.unwrap_or(pos);
        let mut teleported_to: Option<Vec3> = None;

        for (entry_entity, exit_entity, entry, exit) in pairs {
            if lockout.0.contains(&entry_entity) {
                continue;
            }

            let entry_normal = (entry.rotation * Vec3::Y).normalize();
            let prev_along = (prev_pos - entry.translation).dot(entry_normal);
            let curr_along = (pos - entry.translation).dot(entry_normal);
            let crossed = (prev_along > 0.0) != (curr_along > 0.0);
            if !crossed {
                continue;
            }

            let denom = curr_along - prev_along;
            let t = if denom.abs() > 1e-6 {
                (-prev_along / denom).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let cross_pos = prev_pos.lerp(pos, t);
            let to_cross = cross_pos - entry.translation;
            let cross_radial = (to_cross - entry_normal * to_cross.dot(entry_normal)).length();
            if cross_radial > PORTAL_RADIUS {
                continue;
            }

            let basis_pos = if prev_along > 0.0 { prev_pos } else { pos };
            let to_basis = basis_pos - entry.translation;
            let entry_inv = entry.rotation.inverse();
            let q_pos = exit.rotation * entry_inv;
            let vel_flip = Quat::from_rotation_x(core::f32::consts::PI);
            let q_vel = exit.rotation * vel_flip * entry_inv;
            // Write avian Position (canonical) AND Transform so the
            // teleport sticks under LightyearAvianPlugin's sync — the
            // plugin overwrites Transform from Position each tick, so
            // writing Transform alone would be reverted on the next
            // tick. Update Transform too for any system that reads it
            // before the next sync runs.
            let new_pos = exit.translation + q_pos * to_basis;
            tf.translation = new_pos;
            tf.rotation = q_pos * tf.rotation;
            avian_pos.0 = new_pos;
            velocity.0 = q_vel * velocity.0;

            lockout.0.insert(entry_entity);
            lockout.0.insert(exit_entity);
            teleported_to = Some(new_pos);
            break;
        }

        prev.0 = Some(teleported_to.unwrap_or(pos));
    }
}

// --- Teleport: player ----------------------------------------------------

// Retired client-side for the same reason as `process_traveler_teleports`.
// Re-enabling needs the position-overwrite oscillation fix described in
// the IrisPlugin scheduling comment above.
#[allow(dead_code)]
fn process_teleports(
    portals: Query<(Entity, &Portal, &Transform)>,
    mut player: Query<
        (
            &mut Transform,
            &mut Position,
            &mut LinearVelocity,
            &mut Facing,
            &mut PrevPos,
            &mut PortalLockout,
        ),
        (With<Player>, Without<Portal>),
    >,
) {
    for (mut player_tf, mut player_avian_pos, mut player_vel, mut facing, mut prev, mut lockout) in
        &mut player
    {
        let player_pos = player_tf.translation;
        let cam_pos = player_pos + CAMERA_OFFSET;

        lockout.0.retain(|entity| match portals.get(*entity) {
            Ok((_, _, tf)) => (cam_pos - tf.translation).length() <= PORTAL_LOCKOUT_RADIUS,
            Err(_) => false,
        });

        let mut primary: Option<(Entity, Transform)> = None;
        let mut secondary: Option<(Entity, Transform)> = None;
        for (e, portal, tf) in &portals {
            match portal.slot {
                PortalSlot::Primary => primary = Some((e, *tf)),
                PortalSlot::Secondary => secondary = Some((e, *tf)),
            }
        }
        let (Some(primary), Some(secondary)) = (primary, secondary) else {
            prev.0 = Some(player_pos);
            continue;
        };

        let Some(prev_body_pos) = prev.0 else {
            prev.0 = Some(player_pos);
            continue;
        };

        let mut teleported = false;
        for (entry_entity, exit_entity, entry, exit) in [
            (primary.0, secondary.0, &primary.1, &secondary.1),
            (secondary.0, primary.0, &secondary.1, &primary.1),
        ] {
            if lockout.0.contains(&entry_entity) {
                continue;
            }

            let entry_normal = (entry.rotation * Vec3::Y).normalize();
            let is_horizontal = entry_normal.y.abs() > HORIZONTAL_NORMAL_DOT_Y;

            let anchor_offset = if is_horizontal {
                Vec3::ZERO
            } else {
                CAMERA_OFFSET
            };
            let prev_anchor = prev_body_pos + anchor_offset;
            let curr_anchor = player_pos + anchor_offset;

            let prev_rel = prev_anchor - entry.translation;
            let curr_rel = curr_anchor - entry.translation;
            let prev_along = prev_rel.dot(entry_normal);
            let curr_along = curr_rel.dot(entry_normal);

            let (crossed, target_along) = if is_horizontal {
                ((prev_along > 0.0) != (curr_along > 0.0), 0.0)
            } else {
                let r = PLAYER_CAPSULE_RADIUS;
                (
                    prev_along.abs() > r && curr_along.abs() <= r,
                    r * prev_along.signum(),
                )
            };
            if !crossed {
                continue;
            }

            let denom = curr_along - prev_along;
            let t = if denom.abs() > 1e-6 {
                ((target_along - prev_along) / denom).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let cross_anchor_pos = prev_anchor.lerp(curr_anchor, t);
            let to_cross = cross_anchor_pos - entry.translation;
            let cross_radial = (to_cross - entry_normal * to_cross.dot(entry_normal)).length();
            if cross_radial > PORTAL_RADIUS {
                continue;
            }

            let cam_world_rot = Quat::from_axis_angle(Vec3::Y, facing.yaw)
                * Quat::from_axis_angle(Vec3::X, facing.pitch);
            let basis_tf = Transform {
                translation: cross_anchor_pos,
                rotation: cam_world_rot,
                scale: Vec3::ONE,
            };

            let virtual_tf = portal_virtual_transform(basis_tf, entry, exit);
            let mut new_body_pos = virtual_tf.translation - anchor_offset;

            let exit_normal = (exit.rotation * Vec3::Y).normalize();
            let exit_is_horizontal = exit_normal.y.abs() > HORIZONTAL_NORMAL_DOT_Y;
            if exit_is_horizontal && !is_horizontal {
                let stand_offset = (PLAYER_CAPSULE_HALF_HEIGHT + PLAYER_CAPSULE_RADIUS)
                    * exit_normal.y.signum();
                new_body_pos.y = exit.translation.y + stand_offset;
            }

            // Stage Q: write avian's Position (canonical) so
            // LightyearAvianPlugin's sync doesn't snap the body back to
            // the pre-teleport spot. Mirror to Transform for any system
            // that reads it before the next sync.
            player_tf.translation = new_body_pos;
            player_avian_pos.0 = new_body_pos;

            let prev_yaw = facing.yaw;
            let new_fwd = virtual_tf.rotation * Vec3::NEG_Z;
            let horiz_len_sq = new_fwd.x * new_fwd.x + new_fwd.z * new_fwd.z;
            if horiz_len_sq > 1e-6 {
                let inv = horiz_len_sq.sqrt().recip();
                facing.yaw = (-new_fwd.x * inv).atan2(-new_fwd.z * inv);
            }
            const PITCH_LIMIT: f32 = 85.0_f32 * core::f32::consts::PI / 180.0;
            facing.pitch = new_fwd
                .y
                .clamp(-1.0, 1.0)
                .asin()
                .clamp(-PITCH_LIMIT, PITCH_LIMIT);

            if entry_normal.y.abs() > HORIZONTAL_NORMAL_DOT_Y {
                let yaw_delta = facing.yaw - prev_yaw;
                let yaw_rot = Quat::from_axis_angle(Vec3::Y, yaw_delta);
                player_vel.0 = yaw_rot * player_vel.0;
            } else {
                player_vel.0 = Vec3::ZERO;
            }

            lockout.0.insert(entry_entity);
            lockout.0.insert(exit_entity);

            prev.0 = Some(player_tf.translation);
            teleported = true;
            break;
        }

        if !teleported {
            prev.0 = Some(player_pos);
        }
    }
}
