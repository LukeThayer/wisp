//! Custom cast handlers for the Lantern spell. Both `lantern.throw` and
//! `lantern.pickup` route through the server: the client computes the
//! intent (where to throw / where the player is), sends a message, the
//! server applies the action authoritatively, and the resulting entity
//! state replicates back to every connected client.
//!
//! There's no local lantern spawn — single-player without a server doesn't
//! get a lantern. The trade-off matches the rest of Stage P+: server is the
//! source of truth for shared world state.
//!
//! Tunables (throw forward speed, vertical kick, lantern-spawn distance)
//! stay close to the previous single-player values so feel is unchanged.

use bevy::ecs::system::SystemState;
use bevy::prelude::*;
use lightyear::prelude::MessageSender;

use crate::net::protocol::{
    PickupLanternMessage, PlayerInputChannel, ThrowLanternMessage,
};
use crate::player::{LocalPlayer, Player, PlayerCamera};
use crate::spells::data::HandlerId;
use crate::spells::handlers::{CastContext, HandlerRegistry};

const THROW_FORWARD: f32 = 9.0;
const THROW_UP: f32 = 3.0;
/// Distance ahead of the camera to spawn the lantern. Large enough to
/// clear the player's 0.4-radius capsule even at extreme camera angles.
const SPAWN_OFFSET_FORWARD: f32 = 1.5;

pub fn register(app: &mut App) {
    let mut registry = app.world_mut().resource_mut::<HandlerRegistry>();
    registry.register(HandlerId("lantern.throw".to_string()), throw_handler);
    registry.register(HandlerId("lantern.pickup".to_string()), pickup_handler);
}

fn throw_handler(world: &mut World, _ctx: &CastContext) {
    let mut sys_state: SystemState<(
        Single<&GlobalTransform, (With<PlayerCamera>, With<LocalPlayer>)>,
        Option<Single<&mut MessageSender<ThrowLanternMessage>>>,
    )> = SystemState::new(world);
    let (cam_tf, sender) = sys_state.get_mut(world);
    let Some(mut sender) = sender else {
        return;
    };

    let cam_pos = cam_tf.translation();
    let forward = cam_tf.rotation() * Vec3::NEG_Z;
    let origin = cam_pos + forward * SPAWN_OFFSET_FORWARD;
    let velocity = forward * THROW_FORWARD + Vec3::Y * THROW_UP;
    let _ = sender.send::<PlayerInputChannel>(ThrowLanternMessage {
        origin: [origin.x, origin.y, origin.z],
        velocity: [velocity.x, velocity.y, velocity.z],
    });
}

fn pickup_handler(world: &mut World, _ctx: &CastContext) {
    let mut sys_state: SystemState<(
        Single<&Transform, (With<Player>, With<LocalPlayer>)>,
        Option<Single<&mut MessageSender<PickupLanternMessage>>>,
    )> = SystemState::new(world);
    let (player_tf, sender) = sys_state.get_mut(world);
    let Some(mut sender) = sender else {
        return;
    };
    let p = player_tf.translation;
    let _ = sender.send::<PlayerInputChannel>(PickupLanternMessage {
        player_position: [p.x, p.y, p.z],
    });
}
