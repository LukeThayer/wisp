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
    mut player: Single<(&Facing, &mut avian3d::prelude::Rotation), With<Player>>,
    mut cam: Single<&mut Transform, (With<PlayerCamera>, Without<Player>)>,
) {
    // Stage Q: `LightyearAvianPlugin` disables avian's
    // `PhysicsTransformPlugin` and replaces it with its own sync. We
    // can't write `Transform.rotation` directly anymore — the sync
    // would overwrite it with avian's `Rotation` next tick. Write to
    // avian's component instead and let the sync push it to Transform.
    // The camera is NOT a physics body (no `Rotation`), so its child
    // Transform write still works for pitch.
    let (facing, body_rot) = &mut *player;
    body_rot.0 = Quat::from_axis_angle(Vec3::Y, facing.yaw);
    cam.rotation = Quat::from_axis_angle(Vec3::X, facing.pitch);
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

pub fn ground_check(mut q: Query<(&mut Facing, &RayHits), With<Player>>) {
    for (mut facing, hits) in &mut q {
        facing.grounded = !hits.is_empty();
    }
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
