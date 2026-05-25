//! Handheld Portal: hold Fire (or FireSecondary) to wind up via the
//! engine's `Hold + Charging` semantics; once `max_charge` promotes the
//! cast to `Channeling`, the client streams `HoldPortalMessage` to the
//! server each frame. The server spawns a real, replicated
//! `NetworkedPortal` (tagged `HandheldPortal`) and updates its pose
//! every tick from the carrier's camera pose — through-portal rendering
//! and server-authoritative teleport behave identically to placed
//! portals, so the disc is "fully functional" the moment Channeling
//! begins. Releasing the spell button sends one trailing
//! `active: false` and the server despawns the portal.
//!
//! Rotation handling: `NetworkedPortal.normal` updates aren't reliable
//! per CLAUDE.md, so the server packs the carrier's yaw + pitch into
//! `NetworkedPosition` (a per-tick-reliable channel) and the client
//! re-derives `Transform.rotation` via [`sync_handheld_portal_rotation`].
//! Placed portals don't carry the `HandheldPortal` marker, so they
//! skip this system and keep their spawn-time rotation.

use bevy::prelude::*;
use lightyear::prelude::MessageSender;

use crate::net::protocol::{
    HandheldPortal, HoldPortalMessage, NetworkedPortal, NetworkedPosition, PlayerInputChannel,
};
use crate::player::{Facing, LocalPlayer, Player, PlayerCamera};
use crate::spells::data::HandlerId;
use crate::spells::engine::{CastPhase, CastState};
use crate::spells::handlers::{CastContext, HandlerRegistry};
use crate::spells::portal::{disc_rotation, PortalSlot};

const PRIMARY_CAST_ID: &str = "handheld_portal.hold_primary";
const SECONDARY_CAST_ID: &str = "handheld_portal.hold_secondary";

/// Distance in front of the camera where the held portal floats. Far
/// enough to clear the carrier's capsule (radius 0.4) at any aim angle.
const HELD_PORTAL_DISTANCE: f32 = 2.0;

pub fn register(app: &mut App) {
    let mut registry = app.world_mut().resource_mut::<HandlerRegistry>();
    // The cast engine still dispatches a payload while in Channeling,
    // so a handler must be registered or the catalog validator warns.
    // The actual per-tick send lives in `drive_handheld_portal_messages`
    // because we need to detect the trailing release (when the cast
    // transitions out of Channeling, dispatch_casts doesn't fire).
    registry.register(HandlerId(PRIMARY_CAST_ID.to_string()), no_op_handler);
    registry.register(HandlerId(SECONDARY_CAST_ID.to_string()), no_op_handler);
    app.add_plugins(HandheldPortalPlugin);
}

fn no_op_handler(_world: &mut World, _ctx: &CastContext) {}

struct HandheldPortalPlugin;

impl Plugin for HandheldPortalPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<HandheldTx>().add_systems(
            Update,
            (drive_handheld_portal_messages, sync_handheld_portal_rotation),
        );
    }
}

/// Per-slot "was channeling last frame" memory. Lets us emit a single
/// trailing `active: false` on the transition out of Channeling without
/// streaming "off" messages indefinitely. Same shape as
/// `LocalChargeTx` for the charge-orb broadcast.
#[derive(Resource, Default)]
struct HandheldTx {
    primary_active: bool,
    secondary_active: bool,
}

fn drive_handheld_portal_messages(
    mut tx: ResMut<HandheldTx>,
    player: Option<Single<(&CastState, &Facing), (With<Player>, With<LocalPlayer>)>>,
    camera: Option<Single<&GlobalTransform, (With<PlayerCamera>, With<LocalPlayer>)>>,
    sender: Option<Single<&mut MessageSender<HoldPortalMessage>>>,
) {
    let Some(player) = player else { return };
    let Some(camera) = camera else { return };
    let Some(mut sender) = sender else { return };
    let (cast_state, facing) = *player;

    let cam_pos = camera.translation();
    let forward = camera.rotation() * Vec3::NEG_Z;
    let portal_pos = cam_pos + forward * HELD_PORTAL_DISTANCE;

    // Two slots, identical handling — local closure folds the
    // "channeling now? was channeling last frame?" branching into one
    // place per slot.
    let mut handle_slot =
        |slot: PortalSlot, cast_id: &str, prev_active: &mut bool| {
            let is_channeling = cast_state
                .instances
                .get(cast_id)
                .map(|i| i.phase == CastPhase::Channeling)
                .unwrap_or(false);
            if is_channeling {
                let _ = sender.send::<PlayerInputChannel>(HoldPortalMessage {
                    slot,
                    active: true,
                    position: [portal_pos.x, portal_pos.y, portal_pos.z],
                    yaw: facing.yaw,
                    pitch: facing.pitch,
                });
            } else if *prev_active {
                let _ = sender.send::<PlayerInputChannel>(HoldPortalMessage {
                    slot,
                    active: false,
                    position: [0.0; 3],
                    yaw: 0.0,
                    pitch: 0.0,
                });
            }
            *prev_active = is_channeling;
        };
    handle_slot(PortalSlot::Primary, PRIMARY_CAST_ID, &mut tx.primary_active);
    handle_slot(
        PortalSlot::Secondary,
        SECONDARY_CAST_ID,
        &mut tx.secondary_active,
    );
}

/// For each `HandheldPortal`, recompute `Transform.rotation` from the
/// replicated yaw + pitch every time `NetworkedPosition` changes
/// (every server tick during a hold). Placed portals are filtered out
/// by `With<HandheldPortal>` so they keep the rotation
/// `on_networked_portal_replicated` set at spawn time.
fn sync_handheld_portal_rotation(
    mut q: Query<
        (&NetworkedPosition, &mut Transform),
        (
            With<HandheldPortal>,
            With<NetworkedPortal>,
            Changed<NetworkedPosition>,
        ),
    >,
) {
    for (np, mut tf) in &mut q {
        let yaw_q = Quat::from_axis_angle(Vec3::Y, np.yaw);
        let pitch_q = Quat::from_axis_angle(Vec3::X, np.pitch);
        let forward = (yaw_q * pitch_q) * Vec3::NEG_Z;
        tf.rotation = disc_rotation(-forward);
    }
}
