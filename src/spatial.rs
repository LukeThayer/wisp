//! Spatial magic: paired portals. The player has two slots — Primary (left
//! click) and Secondary (right click). Each cast raycasts forward: if it hits
//! anything, the portal sticks to that surface; if not, it floats in the air
//! at a fixed distance. Either click replaces the portal in its own slot,
//! and Primary always pairs with Secondary regardless of how each was placed.

use std::collections::{HashMap, HashSet};

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
use crate::player::{Facing, Player, PlayerCamera};

pub struct SpatialMagicPlugin;

impl Plugin for SpatialMagicPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PrevPlayerPos>()
            .init_resource::<PrevTravelerPos>()
            .init_resource::<LockedPortals>()
            .init_resource::<TravelerLockouts>()
            .add_plugins(MaterialPlugin::<PortalMaterial>::default())
            .add_observer(on_place_primary)
            .add_observer(on_place_secondary)
            .add_systems(
                Update,
                (
                    update_player_falling,
                    update_traveler_falling,
                    process_teleports,
                    process_traveler_teleports,
                    update_portal_cameras,
                    cleanup_orphan_portal_cameras,
                )
                    .chain(),
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

/// Portals that the player has just teleported through and isn't allowed to
/// trigger again. Each entry clears once the player moves past its threshold
/// radius — so you have to actually walk *away* from the portal before it
/// can teleport you again. Stops the bounce-back flicker.
#[derive(Resource, Default)]
pub struct LockedPortals(pub HashSet<Entity>);

/// Marker for any dynamic entity that should traverse portals (props,
/// lanterns, etc.). The player has its own dedicated teleport path because
/// of body/camera split and Facing; everything else uses the simpler
/// `process_traveler_teleports` system.
#[derive(Component)]
pub struct PortalTraveler;

/// Previous-frame world positions for every `PortalTraveler`, keyed by entity.
/// Used the same way `PrevPlayerPos` is — to detect sign-flip crossings.
#[derive(Resource, Default)]
pub struct PrevTravelerPos(pub HashMap<Entity, Vec3>);

/// Per-traveler portal lockouts. A traveler that just teleported through a
/// portal pair has both portals listed here; the entries clear individually
/// as the traveler moves past `PORTAL_LOCKOUT_RADIUS` from each portal. This
/// is separate from `LockedPortals` (which is keyed only on player motion)
/// so each object manages its own bounce-back protection.
#[derive(Resource, Default)]
pub struct TravelerLockouts(pub HashMap<Entity, HashSet<Entity>>);

/// Collision layers for the game. Used so the player can be selectively
/// allowed to phase through the ground when stepping onto / into a portal
/// disc — they actually *fall* through floor portals instead of snapping.
#[derive(PhysicsLayer, Clone, Copy, Debug, Default)]
pub enum GameLayer {
    #[default]
    Default,
    Player,
    Ground,
}

// --- Tuning ---------------------------------------------------------------

pub const PORTAL_RADIUS: f32 = 1.4;
/// Thickness of the visual disc.
const PORTAL_THICKNESS: f32 = 0.04;
/// How far in front of the camera an Air portal is placed.
const AIR_PORTAL_DISTANCE: f32 = 2.5;
const PORTAL_RAYCAST_RANGE: f32 = 15.0;
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
/// Once a portal has teleported the player, it stays locked until they move
/// at least this far from its center. Larger than PORTAL_RADIUS so simply
/// existing at the disc edge doesn't unlock it.
const PORTAL_LOCKOUT_RADIUS: f32 = PORTAL_RADIUS * 1.5;
/// Approximate radius of the player's capsule perpendicular to its axis.
/// Used to fire the teleport when the capsule's face first reaches a
/// vertical portal disc, instead of waiting until the center has already
/// crossed (which lets the camera see briefly past the disc — flicker).
const PLAYER_CAPSULE_RADIUS: f32 = 0.4;
/// Half the length of the capsule's cylindrical section (`Collider::capsule`
/// in `world.rs` uses 1.2). Combined with the radius this gives the
/// half-extent of the capsule along its axis (used to position the body so
/// the feet sit on the exit floor when teleporting through a mismatched-
/// orientation portal pair).
const PLAYER_CAPSULE_HALF_HEIGHT: f32 = 0.6;
/// Camera offset from the player body in world space (the camera child sits
/// at (0, 0.7, 0) locally and the body only yaws around Y, so this stays
/// constant in world). Used to anchor portal teleport math on the camera
/// position rather than the body, matching how the through-view is rendered.
const CAMERA_OFFSET: Vec3 = Vec3::new(0.0, 0.7, 0.0);
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

/// Drop Ground from the player's collision filter when they're inside the
/// threshold cylinder of a *horizontal* portal — floor / ceiling style —
/// so they can actually fall through it. Vertical portals (walls, vertical
/// air discs) don't need this: the disc has no collider, so the player walks
/// through the plane naturally without disabling ground collision.
fn update_player_falling(
    mut commands: Commands,
    locked: Res<LockedPortals>,
    portals: Query<(Entity, &Portal, &GlobalTransform)>,
    player: Query<(Entity, &Transform, &CollisionLayers), With<Player>>,
) {
    let Ok((player_entity, player_tf, layers)) = player.single() else {
        return;
    };

    let mut in_threshold = false;
    for (e, _portal, gt) in &portals {
        if locked.0.contains(&e) {
            continue;
        }
        let portal_tf = gt.compute_transform();
        let normal = (portal_tf.rotation * Vec3::Y).normalize();

        // Only horizontal discs disable ground collision. Otherwise an air
        // portal placed near the player would drop the floor out from under
        // them just by approaching it.
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

/// Same idea as `update_player_falling`, but applied to every `PortalTraveler`.
/// Lets dynamic objects (props, lanterns) fall through floor portals by
/// dropping Ground from their filter when they're inside the disc threshold
/// cylinder of a horizontal portal that this traveler isn't currently
/// locked out of.
fn update_traveler_falling(
    mut commands: Commands,
    traveler_locks: Res<TravelerLockouts>,
    portals: Query<(Entity, &Portal, &GlobalTransform)>,
    travelers: Query<
        (Entity, &Transform, &CollisionLayers),
        (With<PortalTraveler>, Without<Player>),
    >,
) {
    for (traveler_entity, traveler_tf, layers) in &travelers {
        let own_locks = traveler_locks.0.get(&traveler_entity);
        let mut in_threshold = false;
        for (e, _portal, gt) in &portals {
            if own_locks.is_some_and(|s| s.contains(&e)) {
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

        // Wake any traveler resting in the threshold so gravity can pull it
        // through. Without this, an object resting on the floor when a
        // floor portal is placed beneath it just keeps sleeping — its
        // CollisionLayers gets swapped but avian doesn't re-integrate
        // forces on a sleeping body, so the player has to nudge it before
        // it'll start falling.
        if in_threshold {
            commands.entity(traveler_entity).remove::<Sleeping>();
        }
    }
}

/// Generic teleport for `PortalTraveler` entities. Detects center sign-flip
/// against each portal pair and applies a frame-change rotation
/// (`exit.rotation * entry.rotation.inverse()`) to the object's position
/// offset, body rotation, and linear velocity. No `pos_flip`: for two
/// same-orientation portals the object emerges at the same offset (no
/// surprising mirror); for differently-oriented portals the frame change
/// rotates everything coherently. Momentum is preserved because the *same*
/// rotation is applied to position deltas and velocity.
fn process_traveler_teleports(
    mut prev: ResMut<PrevTravelerPos>,
    mut traveler_locks: ResMut<TravelerLockouts>,
    portals: Query<(Entity, &Portal, &Transform)>,
    mut travelers: Query<
        (Entity, &mut Transform, &mut LinearVelocity),
        (With<PortalTraveler>, Without<Portal>, Without<Player>),
    >,
) {
    // Clear per-traveler lockouts for portals each traveler has moved away from.
    for (entity, tf, _) in &travelers {
        let pos = tf.translation;
        if let Some(locks) = traveler_locks.0.get_mut(&entity) {
            locks.retain(|portal_e| match portals.get(*portal_e) {
                Ok((_, _, ptf)) => (pos - ptf.translation).length() <= PORTAL_LOCKOUT_RADIUS,
                Err(_) => false,
            });
        }
    }

    let mut primary: Option<(Entity, Transform)> = None;
    let mut secondary: Option<(Entity, Transform)> = None;
    for (e, portal, tf) in &portals {
        match portal.slot {
            PortalSlot::Primary => primary = Some((e, *tf)),
            PortalSlot::Secondary => secondary = Some((e, *tf)),
        }
    }
    let (Some(primary), Some(secondary)) = (primary, secondary) else {
        let mut next = HashMap::with_capacity(travelers.iter().len());
        for (e, tf, _) in &travelers {
            next.insert(e, tf.translation);
        }
        prev.0 = next;
        return;
    };

    let pairs = [
        (primary.0, secondary.0, &primary.1, &secondary.1),
        (secondary.0, primary.0, &secondary.1, &primary.1),
    ];

    let mut next = HashMap::with_capacity(travelers.iter().len());
    for (entity, mut tf, mut velocity) in &mut travelers {
        let pos = tf.translation;
        let prev_pos = prev.0.get(&entity).copied().unwrap_or(pos);

        let mut teleported_to: Option<Vec3> = None;
        for (entry_entity, exit_entity, entry, exit) in pairs {
            // Per-traveler lockout: this object can't re-use a portal it just
            // teleported through until it's moved away from it.
            if traveler_locks
                .0
                .get(&entity)
                .is_some_and(|s| s.contains(&entry_entity))
            {
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
            let cross_radial =
                (to_cross - entry_normal * to_cross.dot(entry_normal)).length();
            if cross_radial > PORTAL_RADIUS {
                continue;
            }

            // Anchor on the front side of entry so the basis is in front of the
            // disc — the object emerges in front of exit, not straddling it.
            let basis_pos = if prev_along > 0.0 { prev_pos } else { pos };
            let to_basis = basis_pos - entry.translation;
            let entry_inv = entry.rotation.inverse();

            // Two separate frame transforms (this is the Portal-the-game trick):
            // * Position and body rotation: pure frame change from entry to
            //   exit. No flip — keeps height & orientation natural for same-
            //   axis portal pairs.
            // * Velocity: same frame change but with an extra 180° around an
            //   in-plane axis (X), which flips the component along the portal
            //   normal so "into entry" becomes "out of exit". Without this
            //   the lantern enters going forward and emerges going *into* the
            //   exit's back, immediately re-crosses, and visually sticks.
            let q_pos = exit.rotation * entry_inv;
            let vel_flip = Quat::from_rotation_x(core::f32::consts::PI);
            let q_vel = exit.rotation * vel_flip * entry_inv;
            tf.translation = exit.translation + q_pos * to_basis;
            tf.rotation = q_pos * tf.rotation;
            velocity.0 = q_vel * velocity.0;

            // Lock both portals against THIS traveler's re-entry until it
            // walks away from each.
            let locks = traveler_locks.0.entry(entity).or_default();
            locks.insert(entry_entity);
            locks.insert(exit_entity);

            teleported_to = Some(tf.translation);
            break;
        }

        next.insert(entity, teleported_to.unwrap_or(pos));
    }
    prev.0 = next;
}

fn process_teleports(
    mut prev: ResMut<PrevPlayerPos>,
    mut locked: ResMut<LockedPortals>,
    portals: Query<(Entity, &Portal, &Transform)>,
    mut player: Query<
        (&mut Transform, &mut LinearVelocity, &mut Facing),
        (With<Player>, Without<Portal>),
    >,
) {
    let Ok((mut player_tf, mut player_vel, mut facing)) = player.single_mut() else {
        return;
    };
    // Anchor everything on the *camera* position. The through-view's virtual
    // camera is computed from the player camera's world transform, so doing
    // the teleport math from the same anchor keeps the post-teleport view
    // identical to what was being rendered on the disc the frame before.
    let player_pos = player_tf.translation;
    let cam_pos = player_pos + CAMERA_OFFSET;

    // Clear lockouts for any portal the player has moved away from. Doing
    // this first means a same-frame teleport can re-lock if needed.
    locked
        .0
        .retain(|entity| match portals.get(*entity) {
            Ok((_, _, tf)) => (cam_pos - tf.translation).length() <= PORTAL_LOCKOUT_RADIUS,
            // Portal got despawned — drop the lock.
            Err(_) => false,
        });

    // Need both portals to teleport.
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
        return;
    };

    let Some(prev_body_pos) = prev.0 else {
        prev.0 = Some(player_pos);
        return;
    };

    for (entry_entity, exit_entity, entry, exit) in [
        (primary.0, secondary.0, &primary.1, &secondary.1),
        (secondary.0, primary.0, &secondary.1, &primary.1),
    ] {
        // If this portal is locked, the player can't teleport through it
        // again until they leave its threshold radius.
        if locked.0.contains(&entry_entity) {
            continue;
        }

        let entry_normal = (entry.rotation * Vec3::Y).normalize();
        let is_horizontal = entry_normal.y.abs() > HORIZONTAL_NORMAL_DOT_Y;

        // Anchor the math on the body for horizontal portals (so the
        // player's *feet* are what register against the floor disc and they
        // don't have to fall 1.7m before the trigger fires), and on the
        // camera for vertical portals (so post-teleport the camera lands
        // exactly where the through-view was being rendered from — no
        // 0.7m vertical view jump that exposes the top of the disc).
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

        // Pick a trigger condition matched to the orientation:
        // * Horizontal: center sign-flip — the player has actually crossed
        //   the disc plane (after falling through with ground collision
        //   disabled by `update_player_falling`).
        // * Vertical: the capsule's leading face reaches the disc plane so
        //   the camera never spends a frame past the disc.
        let (crossed, target_along) = if is_horizontal {
            (
                (prev_along > 0.0) != (curr_along > 0.0),
                0.0,
            )
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
        let cross_radial =
            (to_cross - entry_normal * to_cross.dot(entry_normal)).length();
        if cross_radial > PORTAL_RADIUS {
            continue;
        }

        // Basis uses the anchor point and the camera's full world rotation,
        // so pitch is preserved through the portal flip math.
        let cam_world_rot = Quat::from_axis_angle(Vec3::Y, facing.yaw)
            * Quat::from_axis_angle(Vec3::X, facing.pitch);
        let basis_tf = Transform {
            translation: cross_anchor_pos,
            rotation: cam_world_rot,
            scale: Vec3::ONE,
        };

        let virtual_tf = portal_virtual_transform(basis_tf, entry, exit);
        let mut new_body_pos = virtual_tf.translation - anchor_offset;

        // Mismatched-orientation portal pairs (e.g. air → floor) map the
        // entry's "distance from disc" into the exit's normal axis, which
        // for a horizontal exit means the player's body Y comes out near
        // the disc itself — buried in the floor. Override Y so the body
        // sits at standing height above the exit disc (or hanging height
        // below a ceiling). Horizontal-entry → horizontal-exit pairs
        // already land at a sensible Y from the math, so this only kicks
        // in when the entry was vertical.
        let exit_normal = (exit.rotation * Vec3::Y).normalize();
        let exit_is_horizontal = exit_normal.y.abs() > HORIZONTAL_NORMAL_DOT_Y;
        if exit_is_horizontal && !is_horizontal {
            let stand_offset = (PLAYER_CAPSULE_HALF_HEIGHT + PLAYER_CAPSULE_RADIUS)
                * exit_normal.y.signum();
            new_body_pos.y = exit.translation.y + stand_offset;
        }

        player_tf.translation = new_body_pos;

        // Match the virtual camera's full look direction — both yaw and
        // pitch. Pitch only updated when there's a horizontal component to
        // anchor yaw against; otherwise (straight up / down) keep the
        // player's old yaw to avoid a flip-around.
        let prev_yaw = facing.yaw;
        let new_fwd = virtual_tf.rotation * Vec3::NEG_Z;
        let horiz_len_sq = new_fwd.x * new_fwd.x + new_fwd.z * new_fwd.z;
        if horiz_len_sq > 1e-6 {
            let inv = horiz_len_sq.sqrt().recip();
            facing.yaw = (-new_fwd.x * inv).atan2(-new_fwd.z * inv);
        }
        // pitch = asin(forward.y) for a unit vector. Clamp to the controller's
        // ±85° limit so we don't end up at a singularity the player can't
        // un-tilt with mouse input.
        const PITCH_LIMIT: f32 = 85.0_f32 * core::f32::consts::PI / 180.0;
        facing.pitch = new_fwd
            .y
            .clamp(-1.0, 1.0)
            .asin()
            .clamp(-PITCH_LIMIT, PITCH_LIMIT);

        // For *horizontal* (floor / ceiling) entries we want momentum to
        // carry through — that's what makes the hole feel like a hole, you
        // keep falling out the other side. For wall entries, zero velocity
        // to avoid the "thrown" feel when the portal rotation flips your
        // world direction.
        if entry_normal.y.abs() > HORIZONTAL_NORMAL_DOT_Y {
            let yaw_delta = facing.yaw - prev_yaw;
            let yaw_rot = Quat::from_axis_angle(Vec3::Y, yaw_delta);
            player_vel.0 = yaw_rot * player_vel.0;
        } else {
            player_vel.0 = Vec3::ZERO;
        }

        // Lock both portals until the player walks away from each.
        locked.0.insert(entry_entity);
        locked.0.insert(exit_entity);

        prev.0 = Some(player_tf.translation);
        return;
    }

    prev.0 = Some(player_pos);
}
