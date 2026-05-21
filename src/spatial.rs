//! Spatial magic: paired portals. The player has two slots — Primary (left
//! click) and Secondary (right click). Each cast raycasts forward: if it hits
//! anything, the portal sticks to that surface; if not, it floats in the air
//! at a fixed distance. Either click replaces the portal in its own slot,
//! and Primary always pairs with Secondary regardless of how each was placed.

use avian3d::prelude::*;
use bevy::camera::RenderTarget;
use bevy::pbr::{Material, MaterialPlugin};
use bevy::prelude::*;
use bevy::render::render_resource::{AsBindGroup, TextureFormat};
use bevy::shader::ShaderRef;
use bevy::window::PrimaryWindow;
use bevy_enhanced_input::prelude::*;

use crate::input::{Fire, FireSecondary};
use crate::magic::Lantern;
use crate::player::spells::{ActiveSpell, SpellId};
use crate::player::{Player, PlayerCamera};

pub struct SpatialMagicPlugin;

impl Plugin for SpatialMagicPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PrevPlayerPos>()
            .add_plugins(MaterialPlugin::<PortalMaterial>::default())
            .add_observer(on_place_primary)
            .add_observer(on_place_secondary)
            .add_systems(
                Update,
                (
                    process_teleports,
                    update_portal_cameras,
                    cleanup_orphan_portal_cameras,
                ),
            );
    }
}

/// Material applied to portal discs. The fragment shader samples
/// `portal_tex` in screen space, so the disc looks like a window into the
/// rendered view rather than a UV-mapped sticker.
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

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PortalSlot {
    Primary,
    Secondary,
}

#[derive(Component)]
pub struct Portal {
    pub slot: PortalSlot,
}

/// Marker on the off-screen Camera3d that renders the through-portal view for
/// `portal`. Each placed portal owns one of these; the camera is despawned
/// when its portal goes away.
#[derive(Component)]
pub struct PortalCamera {
    pub portal: Entity,
}

/// Player position from the previous frame, used to detect when the player's
/// position has actually *crossed* a portal's disc plane (rather than just
/// being near it). Standing on the boundary keeps `prev_along` and
/// `curr_along` on the same side, so no teleport fires.
#[derive(Resource, Default)]
pub struct PrevPlayerPos(pub Option<Vec3>);

// --- Tuning ---------------------------------------------------------------

const PORTAL_RADIUS: f32 = 1.4;
/// Thickness of the visual disc.
const PORTAL_THICKNESS: f32 = 0.04;
/// How far in front of the camera an Air portal is placed.
const AIR_PORTAL_DISTANCE: f32 = 2.5;
const PORTAL_RAYCAST_RANGE: f32 = 15.0;
/// How far in front of the exit portal to spawn the player.
const EXIT_OFFSET: f32 = 0.9;
/// Minimum exit speed so the player isn't left straddling the destination.
const EXIT_MIN_SPEED: f32 = 3.0;
/// If a portal's normal has more vertical than this, we treat it as a
/// horizontal disc (floor / ceiling) and use the "stepped onto the disc"
/// trigger instead of a plane-crossing trigger — the player's capsule center
/// can't actually cross below a floor portal's plane because the floor
/// collider blocks them.
const HORIZONTAL_NORMAL_DOT_Y: f32 = 0.7;
/// Max vertical distance from a horizontal disc at which "stepping onto it"
/// still counts as entering it. Lets the player walk onto a floor portal at
/// normal standing height but not trigger from a story above.
const HORIZONTAL_DISC_DEPTH: f32 = 2.0;
/// Tiny offset off the hit surface to avoid z-fighting with the wall.
const SURFACE_INSET: f32 = 0.02;
/// Cap on the portal render texture's longer edge so we don't allocate huge
/// targets on very large displays.
const PORTAL_TEXTURE_MAX_DIM: u32 = 1280;
/// Vertical FOV for the portal cameras — matches the player's main camera.
const PORTAL_CAMERA_FOV_DEG: f32 = 90.0;

/// Pick a render-texture resolution that mirrors the window's aspect ratio
/// (capped at PORTAL_TEXTURE_MAX_DIM on the longest side).
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

pub fn on_place_primary(
    _: On<Start<Fire>>,
    active_spell: Res<ActiveSpell>,
    commands: Commands,
    meshes: ResMut<Assets<Mesh>>,
    portal_materials: ResMut<Assets<PortalMaterial>>,
    images: ResMut<Assets<Image>>,
    spatial: SpatialQuery,
    cam: Single<&GlobalTransform, With<PlayerCamera>>,
    window: Single<&Window, With<PrimaryWindow>>,
    player_entity: Single<Entity, With<Player>>,
    portals: Query<(Entity, &Portal)>,
    lanterns: Query<Entity, With<Lantern>>,
) {
    if active_spell.0 != SpellId::Portal {
        return;
    }
    place_portal(
        PortalSlot::Primary,
        commands,
        meshes,
        portal_materials,
        images,
        &spatial,
        **cam,
        &window,
        *player_entity,
        portals,
        lanterns,
    );
}

pub fn on_place_secondary(
    _: On<Start<FireSecondary>>,
    active_spell: Res<ActiveSpell>,
    commands: Commands,
    meshes: ResMut<Assets<Mesh>>,
    portal_materials: ResMut<Assets<PortalMaterial>>,
    images: ResMut<Assets<Image>>,
    spatial: SpatialQuery,
    cam: Single<&GlobalTransform, With<PlayerCamera>>,
    window: Single<&Window, With<PrimaryWindow>>,
    player_entity: Single<Entity, With<Player>>,
    portals: Query<(Entity, &Portal)>,
    lanterns: Query<Entity, With<Lantern>>,
) {
    if active_spell.0 != SpellId::Portal {
        return;
    }
    place_portal(
        PortalSlot::Secondary,
        commands,
        meshes,
        portal_materials,
        images,
        &spatial,
        **cam,
        &window,
        *player_entity,
        portals,
        lanterns,
    );
}

#[allow(clippy::too_many_arguments)]
fn place_portal(
    slot: PortalSlot,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut portal_materials: ResMut<Assets<PortalMaterial>>,
    mut images: ResMut<Assets<Image>>,
    spatial: &SpatialQuery,
    cam: GlobalTransform,
    window: &Window,
    player_entity: Entity,
    portals: Query<(Entity, &Portal)>,
    lanterns: Query<Entity, With<Lantern>>,
) {
    let cam_pos = cam.translation();
    let forward = cam.rotation() * Vec3::NEG_Z;

    // Raycast: if we hit something, surface-style placement; otherwise air.
    let (pos, normal) = if let Ok(dir) = Dir3::new(forward) {
        let excluded: Vec<Entity> = lanterns
            .iter()
            .chain(portals.iter().map(|(e, _)| e))
            .chain(std::iter::once(player_entity))
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

    // Replace any existing portal in this slot.
    for (e, portal) in &portals {
        if portal.slot == slot {
            commands.entity(e).despawn();
        }
    }

    let (rim_color, name) = match slot {
        PortalSlot::Primary => (LinearRgba::rgb(1.5, 0.55, 0.12), "Portal-Primary"),
        PortalSlot::Secondary => (LinearRgba::rgb(0.25, 0.85, 2.2), "Portal-Secondary"),
    };

    let rotation = disc_rotation(normal);

    let (tex_w, tex_h) = portal_texture_size(window);
    let render_image = images.add(Image::new_target_texture(
        tex_w,
        tex_h,
        TextureFormat::Bgra8UnormSrgb,
        None,
    ));

    let portal_entity = commands
        .spawn((
            Name::new(name),
            Portal { slot },
            Mesh3d(meshes.add(Cylinder::new(PORTAL_RADIUS, PORTAL_THICKNESS))),
            MeshMaterial3d(portal_materials.add(PortalMaterial {
                rim_color,
                portal_tex: render_image.clone(),
            })),
            Transform::from_translation(pos).with_rotation(rotation),
        ))
        .id();

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
    ));
}

// --- Teleport -------------------------------------------------------------

// --- Through-portal rendering --------------------------------------------

/// Build a stable rotation for a portal disc whose face normal is `normal`.
///
/// The cylinder mesh's axis is local +Y, so local +Y is mapped to the normal.
/// The remaining two axes are constructed so local +Z is the projection of
/// world up onto the disc plane (i.e. the disc's "up" stays world-up-aligned
/// no matter what direction the player was facing on placement). For floor
/// and ceiling portals where the normal is parallel to world up, we fall
/// back to using world +Z as the disc up.
fn disc_rotation(normal: Vec3) -> Quat {
    let y_axis = normal.normalize_or_zero();
    if y_axis == Vec3::ZERO {
        return Quat::IDENTITY;
    }

    let world_up = Vec3::Y;
    let z_axis = if y_axis.cross(world_up).length_squared() < 1e-6 {
        // Normal is (anti-)parallel to world up; pick a stable horizontal.
        Vec3::Z
    } else {
        let projected = world_up - y_axis * y_axis.dot(world_up);
        projected.normalize()
    };
    let x_axis = y_axis.cross(z_axis);

    Quat::from_mat3(&Mat3::from_cols(x_axis, y_axis, z_axis))
}

/// Build the virtual-camera transform that, when rendered, shows the world
/// from the paired portal's vantage.
///
/// We use the portal-fold math only to compute **position** and the **forward
/// direction**; the camera's roll is horizon-stabilized to world up so the
/// rendered image stays upright regardless of the disc's in-plane orientation
/// or how the player was angled when they placed the portals.
///
/// * **Position**: 180° around the portal's up axis (local +Y) — keeps the
///   across-portal coordinate intact while mirroring the in-plane axes, the
///   standard portal mirror.
/// * **Forward**: 180° around an in-plane axis (local +Z) on the player's
///   look direction, then mapped into exit-local — flips the "into portal"
///   direction into "out of portal." We use Z rather than X because the Z
///   axis of `disc_rotation` is the world-up-projected one; flipping around
///   it preserves the player's yaw direction through the portal (turn right,
///   the through-view turns right).
/// * **Roll**: built with `look_to(forward, world_up)` so world up stays up.
///   When forward is parallel to world up (e.g. a floor portal exit viewed
///   through a vertical air portal), the player's horizontal look direction
///   is used as the up reference instead, so the view rotates with player
///   yaw rather than snapping to an arbitrary axis.
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

    // Pick an up-reference. Prefer world up; if the camera is looking straight
    // up or down, use the player's horizontal heading so the rendered view
    // tracks the player's yaw smoothly.
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
    player_cam: Single<&GlobalTransform, With<PlayerCamera>>,
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
            // Belongs to a portal that's already been replaced; cleanup
            // system will despawn it next frame.
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

fn process_teleports(
    mut prev: ResMut<PrevPlayerPos>,
    portals: Query<(&Portal, &Transform)>,
    mut player: Query<(&mut Transform, &mut LinearVelocity), (With<Player>, Without<Portal>)>,
) {
    let Ok((mut player_tf, mut player_vel)) = player.single_mut() else {
        return;
    };
    let player_pos = player_tf.translation;

    // Need both portals to teleport.
    let mut primary: Option<Transform> = None;
    let mut secondary: Option<Transform> = None;
    for (portal, tf) in &portals {
        match portal.slot {
            PortalSlot::Primary => primary = Some(*tf),
            PortalSlot::Secondary => secondary = Some(*tf),
        }
    }
    let (Some(primary), Some(secondary)) = (primary, secondary) else {
        prev.0 = Some(player_pos);
        return;
    };

    // Need a previous frame to detect a crossing.
    let Some(prev_pos) = prev.0 else {
        prev.0 = Some(player_pos);
        return;
    };

    for (entry, exit) in [(&primary, &secondary), (&secondary, &primary)] {
        let entry_normal = (entry.rotation * Vec3::Y).normalize();
        let prev_rel = prev_pos - entry.translation;
        let curr_rel = player_pos - entry.translation;
        let prev_along = prev_rel.dot(entry_normal);
        let curr_along = curr_rel.dot(entry_normal);

        let triggered = if entry_normal.y.abs() > HORIZONTAL_NORMAL_DOT_Y {
            // Horizontal disc (floor or ceiling). The player's capsule center
            // can't physically cross the plane, so trigger on a "stepped onto
            // the disc" transition: radial distance just went from outside the
            // disc to inside, while close enough to the plane vertically.
            let prev_radial = (prev_rel - entry_normal * prev_along).length();
            let curr_radial = (curr_rel - entry_normal * curr_along).length();
            prev_radial > PORTAL_RADIUS
                && curr_radial <= PORTAL_RADIUS
                && curr_along.abs() < HORIZONTAL_DISC_DEPTH
        } else {
            // Vertical / oblique disc. Trigger only when the player actually
            // crosses the plane front → back, and the interpolated crossing
            // point is inside the disc.
            if !(prev_along > 0.0 && curr_along <= 0.0) {
                false
            } else {
                let t = prev_along / (prev_along - curr_along);
                let cross_pos = prev_pos.lerp(player_pos, t);
                let to_cross = cross_pos - entry.translation;
                let radial =
                    (to_cross - entry_normal * to_cross.dot(entry_normal)).length();
                radial <= PORTAL_RADIUS
            }
        };

        if !triggered {
            continue;
        }

        let exit_normal = (exit.rotation * Vec3::Y).normalize();
        player_tf.translation = exit.translation + exit_normal * EXIT_OFFSET;
        let speed = player_vel.0.length().max(EXIT_MIN_SPEED);
        player_vel.0 = exit_normal * speed;
        // Anchor the previous-position cache to the post-teleport spot so the
        // next frame doesn't see a phantom "crossing" of the exit portal.
        prev.0 = Some(player_tf.translation);
        return;
    }

    prev.0 = Some(player_pos);
}
