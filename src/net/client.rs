//! Client-side network plugin. Layers lightyear's `ClientPlugins` plus our
//! shared `ProtocolPlugin`. On Startup it spawns a netcode client entity
//! that tries to connect to `default_server_addr()`. An observer logs the
//! connection event.

use core::time::Duration;
use std::net::SocketAddr;

use bevy::prelude::*;
use lightyear::netcode::prelude::Authentication;
use lightyear::prelude::client::{ClientPlugins, Connect, NetcodeClient, NetcodeConfig};
use lightyear::prelude::{
    Connected, LocalAddr, LocalId, MessageSender, PeerAddr, ReplicationReceiver, UdpIo,
};

use crate::input::{Jump, Movement};
use crate::net::protocol::{
    CustomizeMessage, PlayerCustomization, PlayerInputChannel, PlayerInputMessage,
};
use crate::player::parts::PartSelection;
use crate::player::recolor::CharacterColors;
use crate::net::replication::{ReplicationLocalPlugin, ReplicationTracePlugin};
use crate::net::{default_server_addr, ProtocolPlugin, NETCODE_KEY, PROTOCOL_ID, TICK_HZ};
use crate::player::{Facing, LocalPlayer, Player};
use crate::trace;
use bevy_enhanced_input::prelude::Action;
use serde_json::json;

pub struct ClientNetPlugin;

impl Plugin for ClientNetPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ClientPlugins {
            tick_duration: Duration::from_secs_f32(1.0 / TICK_HZ as f32),
        })
        .add_plugins(ProtocolPlugin)
        .add_plugins(ReplicationLocalPlugin)
        .add_plugins(ReplicationTracePlugin)
        .add_plugins(trace::TracePlugin)
        .insert_resource(ConnectTo {
            server: default_server_addr(),
            client_id: pseudo_unique_client_id(),
        })
        .add_systems(Startup, spawn_client)
        .add_systems(Update, (send_local_player_input, send_local_customization))
        .add_observer(on_connected);
    }
}

/// Stores the connection target so it can be overridden by CLI before
/// `spawn_client` runs.
#[derive(Resource, Clone)]
pub struct ConnectTo {
    pub server: SocketAddr,
    pub client_id: u64,
}

/// Derive a per-process client id from wall-clock nanoseconds so two
/// clients on the same machine don't collide. Dev/test only; production
/// would receive its id from an auth service.
fn pseudo_unique_client_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn spawn_client(mut commands: Commands, target: Res<ConnectTo>) {
    let auth = Authentication::Manual {
        server_addr: target.server,
        client_id: target.client_id,
        private_key: NETCODE_KEY,
        protocol_id: PROTOCOL_ID,
    };
    let client = match NetcodeClient::new(auth, NetcodeConfig::default()) {
        Ok(c) => c,
        Err(err) => {
            error!("Failed to build NetcodeClient: {err:?}");
            return;
        }
    };
    info!(
        "Connecting to server {} as client_id={}…",
        target.server, target.client_id
    );
    // Bind to 0.0.0.0:0 so the OS picks an ephemeral local port — required
    // by lightyear's UdpIo link even on the client side. Two clients on
    // the same machine therefore get distinct local ports automatically.
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    let entity = commands
        .spawn((
            client,
            UdpIo::default(),
            LocalAddr(local_addr),
            PeerAddr(target.server),
            ReplicationReceiver::default(),
        ))
        .id();
    commands.trigger(Connect { entity });
}

/// Each Update tick, broadcast the local player's input to the server.
/// Server runs its authoritative controller against this and ships the
/// resulting pose back via `NetworkedPosition` (Stage Q option b — no
/// lightyear-native rollback; local player runs the same controller for
/// snappy visual response and we don't reconcile, accepting server may
/// disagree slightly).
fn send_local_player_input(
    player: Option<
        Single<(&Facing, &crate::spells::engine::CastState), (With<Player>, With<LocalPlayer>)>,
    >,
    movement_action: Option<Single<&Action<Movement>>>,
    jump_action: Option<Single<&Action<Jump>>>,
    sender: Option<Single<&mut MessageSender<PlayerInputMessage>>>,
) {
    let (Some(player), Some(mut sender)) = (player, sender) else {
        return;
    };
    let (facing, cast_state) = *player;
    let casting = cast_state.instances.values().any(|inst| {
        matches!(
            inst.phase,
            crate::spells::engine::CastPhase::Charging
                | crate::spells::engine::CastPhase::Channeling
        )
    });
    let movement = movement_action
        .map(|m| **m.into_inner())
        .unwrap_or(Vec2::ZERO);
    let jump = jump_action.map(|j| **j.into_inner()).unwrap_or(false);
    let _ = sender.send::<PlayerInputChannel>(PlayerInputMessage {
        movement: [movement.x, movement.y],
        yaw: facing.yaw,
        pitch: facing.pitch,
        jump,
        casting,
    });
}

/// Whenever the local customization (`CharacterColors`,
/// `CurrentCharacter`) changes, ship a `CustomizeMessage` to the
/// server. Server stamps it onto our `NetworkedPlayer.PlayerCustomization`
/// component and component replication broadcasts it to every other
/// peer. Local rendering doesn't wait for the round-trip — the
/// recolor system already reads `CharacterColors` directly for the
/// local body — but every observer will see the new colors once the
/// server roundtrip completes.
fn send_local_customization(
    colors: Res<CharacterColors>,
    parts: Res<PartSelection>,
    sender: Option<Single<&mut MessageSender<CustomizeMessage>>>,
) {
    if !colors.is_changed() && !parts.is_changed() {
        return;
    }
    let Some(mut sender) = sender else {
        return;
    };
    let to_arr = |c: bevy::prelude::LinearRgba| [c.red, c.green, c.blue];
    let customization = PlayerCustomization {
        body: [to_arr(colors.body[0]), to_arr(colors.body[1]), to_arr(colors.body[2])],
        objects: [
            to_arr(colors.objects[0]),
            to_arr(colors.objects[1]),
            to_arr(colors.objects[2]),
        ],
        parts: *parts,
    };
    let _ = sender.send::<PlayerInputChannel>(CustomizeMessage { customization });
}

fn on_connected(
    trigger: On<Add, Connected>,
    local_ids: Query<&LocalId, Without<lightyear::prelude::server::ClientOf>>,
) {
    if let Ok(LocalId(peer_id)) = local_ids.get(trigger.entity) {
        info!("Connected to server: client peer_id={:?}", peer_id);
        trace::event("connected", json!({"peer_id": format!("{:?}", peer_id)}));
    }
}
