use avian3d::prelude::*;
use bevy::{
    prelude::*,
    window::{CursorGrabMode, CursorOptions, PrimaryWindow},
};
use bevy_enhanced_input::prelude::*;

use crate::input::{InputMode, Jump, Look, Movement};
use crate::player::LocalPlayer;

const MAX_SPEED: f32 = 4.2;
const GROUND_ACCEL: f32 = 60.0;
const AIR_ACCEL: f32 = 10.0;
const JUMP_IMPULSE: f32 = 5.0;
const PITCH_LIMIT: f32 = 85.0_f32 * std::f32::consts::PI / 180.0;
/// Idle-brake deceleration when grounded and no movement keys are held.
/// apply_movement only fires while WASD is held, so without this we'd coast
/// on LinearDamping alone — far too slippery.
const BRAKE_ACCEL: f32 = 50.0;

#[derive(Component)]
pub struct Player;

#[derive(Component)]
pub struct PlayerCamera;

#[derive(Component, Default, Debug)]
pub struct Facing {
    pub yaw: f32,
    pub pitch: f32,
    pub grounded: bool,
}

pub fn apply_look(
    look: On<Fire<Look>>,
    mut facing: Single<&mut Facing>,
) {
    facing.yaw += look.value.x;
    facing.pitch = (facing.pitch + look.value.y).clamp(-PITCH_LIMIT, PITCH_LIMIT);
}

/// Stage Q.5b reconciliation: pull the local player rig's position from
/// the server-replicated `NetworkedPlayer` whose `NetworkOwner` matches
/// this client's id. Local rig is `Kinematic` so this Position write is
/// authoritative — avian doesn't try to integrate it. The local rig's
/// `Rotation` stays local (driven by `Facing.yaw` via `apply_rotation`)
/// so mouse-look feels snappy; the body yaw the server uses arrives via
/// the input round-trip so other clients see the matching value.
///
/// Without this, the local rig and the server's authoritative player
/// drift independently — locally you move, but every other peer sees
/// you stuck near spawn, because the server's pose is the one that
/// replicates and the local rig was a separate entity.
pub fn sync_local_player_from_server(
    local_ids: Query<
        &lightyear::prelude::LocalId,
        Without<lightyear::prelude::server::ClientOf>,
    >,
    networked_players: Query<
        (
            &crate::net::protocol::NetworkOwner,
            &crate::net::protocol::NetworkedPosition,
        ),
        With<crate::net::protocol::NetworkedPlayer>,
    >,
    mut local_player: Single<&mut avian3d::prelude::Position, (With<Player>, With<LocalPlayer>)>,
) {
    let Some(my_id) =
        local_ids.iter().next().and_then(|local_id| match local_id.0 {
            lightyear::prelude::PeerId::Netcode(id)
            | lightyear::prelude::PeerId::Steam(id)
            | lightyear::prelude::PeerId::Local(id)
            | lightyear::prelude::PeerId::Entity(id) => Some(id),
            _ => None,
        })
    else {
        return;
    };
    for (owner, netpos) in &networked_players {
        if owner.0 == my_id {
            local_player.0 = Vec3::new(netpos.x, netpos.y, netpos.z);
            return;
        }
    }
}

pub fn apply_rotation(
    mode: Res<InputMode>,
    orbit: Res<crate::ui::customization::OrbitCamera>,
    mut player: Single<(&Facing, &mut avian3d::prelude::Rotation), With<Player>>,
    mut cam: Single<
        (&mut Transform, &mut bevy::camera::visibility::RenderLayers),
        (With<PlayerCamera>, Without<Player>),
    >,
) {
    use crate::player::SELF_BODY_LAYER;
    let (cam_transform, cam_layers) = &mut *cam;
    // Stage Q: `LightyearAvianPlugin` disables avian's
    // `PhysicsTransformPlugin` and replaces it with its own sync. We
    // can't write `Transform.rotation` directly anymore — the sync
    // would overwrite it with avian's `Rotation` next tick. Write to
    // avian's component instead and let the sync push it to Transform.
    // The camera is NOT a physics body (no `Rotation`), so its child
    // Transform write still works for pitch.
    let (facing, body_rot) = &mut *player;
    body_rot.0 = Quat::from_axis_angle(Vec3::Y, facing.yaw);

    if *mode == InputMode::Customizing {
        // 3rd-person orbit: position the camera behind+above the
        // player, looking back at chest height. Player root faces -Z
        // in its own frame, so +Z is the "behind" direction. A/D in
        // customization mode drives `OrbitCamera.yaw` which rotates
        // this offset around the player.
        const CAM_LOCAL_REST: Vec3 = Vec3::new(0.0, 1.3, 3.5);
        const LOOK_AT: Vec3 = Vec3::new(0.0, 0.5, 0.0);
        let orbit_rot = Quat::from_axis_angle(Vec3::Y, orbit.yaw);
        cam_transform.translation = orbit_rot * CAM_LOCAL_REST;
        cam_transform.look_at(LOOK_AT, Vec3::Y);
        // Include the self-body render layer so we can actually see
        // our character. In 1st person this layer is hidden so the
        // camera doesn't end up rendering the inside of our own head.
        **cam_layers = bevy::camera::visibility::RenderLayers::from_layers(&[0, SELF_BODY_LAYER]);
    } else {
        // 1st person: head height, no offset, pitch from facing.
        cam_transform.translation = Vec3::new(0.0, 0.7, 0.0);
        cam_transform.rotation = Quat::from_axis_angle(Vec3::X, facing.pitch);
        **cam_layers = bevy::camera::visibility::RenderLayers::layer(0);
    }
}

pub fn apply_movement(
    movement: On<Fire<Movement>>,
    time: Res<Time>,
    mut q: Query<(&Facing, Forces), With<Player>>,
) {
    let Ok((facing, mut forces)) = q.single_mut() else {
        return;
    };

    let input = movement.value.clamp_length_max(1.0);
    let world_dir =
        Quat::from_axis_angle(Vec3::Y, facing.yaw) * Vec3::new(input.x, 0.0, -input.y);

    let current_vel = forces.linear_velocity();
    let current_ground = Vec3::new(current_vel.x, 0.0, current_vel.z);
    let desired_ground = world_dir * MAX_SPEED;

    let accel_limit = if facing.grounded { GROUND_ACCEL } else { AIR_ACCEL };
    let dt = time.delta_secs().max(1e-5);
    let delta = (desired_ground - current_ground).clamp_length_max(accel_limit * dt);
    forces.apply_linear_acceleration(delta / dt);
}

pub fn apply_jump(
    _: On<Start<Jump>>,
    mut q: Query<(&Facing, &mut LinearVelocity), With<Player>>,
) {
    let Ok((facing, mut velocity)) = q.single_mut() else {
        return;
    };
    if !facing.grounded {
        return;
    }
    velocity.0.y = JUMP_IMPULSE;
}

/// Name of the spine bone that drives upper-body aim lean. The
/// Polysplit rig's spine chain is
/// `pelvis_joint → waist_joint → chest_joint → neck_joint`; rotating
/// `chest_joint` leans the torso, leaving the hips and legs planted.
pub const AIM_PITCH_BONE: &str = "chest_joint";

/// After animation has set bone Transforms, apply the local player's
/// pitch on top of the spine bone so the body leans with the aim.
/// Runs in `PostUpdate`, ordered between `AnimationSystems` and
/// `TransformSystems::Propagate`, so the modification is included in
/// the per-frame `GlobalTransform` propagation.
pub fn apply_aim_pitch_to_local_spine(
    facing: Option<Single<&Facing, (With<Player>, With<LocalPlayer>)>>,
    bones: Query<(Entity, &Name)>,
    parents: Query<&ChildOf>,
    body_marker: Query<(), With<crate::player::LocalWizardBody>>,
    mut transforms: Query<&mut Transform>,
) {
    let Some(facing) = facing else { return };
    // Bone-local axes on the gltf-imported Polysplit chest bone:
    // X runs along the spine (its "up"), so rotating around X
    // twists the torso. Z is the perpendicular sideways axis we
    // pivot the lean around. Negative sign: facing.pitch is
    // positive when looking up, but the chest's +Z faces the
    // wrong way for "lean back" — invert so up-look bends the
    // upper body back, down-look bends it forward.
    let pitch_quat = Quat::from_axis_angle(Vec3::Z, -facing.pitch);
    for (entity, name) in &bones {
        if name.as_str() != AIM_PITCH_BONE {
            continue;
        }
        if !ancestor_has_body_marker(entity, &parents, &body_marker) {
            continue;
        }
        if let Ok(mut tf) = transforms.get_mut(entity) {
            // Post-multiply so the animation's bone rotation is
            // preserved and the aim pitch is added on top in the
            // bone's local frame.
            tf.rotation = tf.rotation * pitch_quat;
        }
    }
}

fn ancestor_has_body_marker(
    entity: Entity,
    parents: &Query<&ChildOf>,
    marker: &Query<(), With<crate::player::LocalWizardBody>>,
) -> bool {
    let mut cur = entity;
    loop {
        if marker.contains(cur) {
            return true;
        }
        match parents.get(cur) {
            Ok(p) => cur = p.0,
            Err(_) => return false,
        }
    }
}

pub fn ground_check(mut q: Query<(&mut Facing, &RayHits), With<Player>>) {
    for (mut facing, hits) in &mut q {
        facing.grounded = !hits.is_empty();
    }
}

/// Derive a synthetic `LinearVelocity` on the local Player rig from
/// position deltas. The rig is `Kinematic` (Stage Q.5b — server owns
/// the authoritative movement and we copy Position via
/// `sync_local_player_from_server`), so avian doesn't integrate
/// velocity for us. Animation systems (`drive_animation` →
/// `apply_locomotion_blend`) read `LinearVelocity` to decide between
/// idle / walk / direction-blended walk clips — without this they'd
/// see zero speed forever and the body would never play the walk
/// animation when viewed through portals.
pub fn track_local_velocity(
    time: Res<Time>,
    mut prev_pos: Local<Option<Vec3>>,
    q: Single<
        (&avian3d::prelude::Position, &mut LinearVelocity),
        (With<Player>, With<LocalPlayer>),
    >,
) {
    let (pos, mut vel) = q.into_inner();
    let current = pos.0;
    if let Some(prev) = *prev_pos {
        let dt = time.delta_secs().max(1e-3);
        let raw = (current - prev) / dt;
        // Exponential smoothing — ALPHA=0.35 reaches ~95% of a step
        // input in ~7 frames (~120ms at 60Hz). Matches the cadence
        // of the locomotion blend so the walk clip doesn't pop in.
        const ALPHA: f32 = 0.35;
        vel.0 = vel.0 * (1.0 - ALPHA) + raw * ALPHA;
    }
    *prev_pos = Some(current);
}

pub fn apply_ground_brake(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    mode: Res<InputMode>,
    mut q: Query<(&Facing, Forces), With<Player>>,
) {
    let Ok((facing, mut forces)) = q.single_mut() else {
        return;
    };
    if !facing.grounded {
        return;
    }

    // Brake whenever movement isn't actively being requested. In the radial
    // menu the WASD keys still register as pressed at the OS level, but the
    // movement context is off — so we ignore them and brake.
    let movement_held = *mode == InputMode::Player
        && (keys.pressed(KeyCode::KeyW)
            || keys.pressed(KeyCode::KeyA)
            || keys.pressed(KeyCode::KeyS)
            || keys.pressed(KeyCode::KeyD));
    if movement_held {
        return;
    }

    let v = forces.linear_velocity();
    let ground_v = Vec3::new(v.x, 0.0, v.z);
    if ground_v.length_squared() < 1e-4 {
        return;
    }

    let dt = time.delta_secs().max(1e-5);
    let brake = ground_v.clamp_length_max(BRAKE_ACCEL * dt);
    forces.apply_linear_acceleration(-brake / dt);
}

pub fn cursor_grab(
    mode: Res<InputMode>,
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    mut cursor: Single<&mut CursorOptions, With<PrimaryWindow>>,
) {
    // The radial menu owns the cursor while it's open; don't fight it.
    if *mode != InputMode::Player {
        return;
    }
    if cursor.visible {
        if mouse.just_pressed(MouseButton::Left) {
            cursor.grab_mode = CursorGrabMode::Locked;
            cursor.visible = false;
        }
    } else if keys.just_pressed(KeyCode::Escape) {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    }
}
