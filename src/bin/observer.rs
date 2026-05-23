//! Headless observer client. Connects to the wisp server, receives
//! replicated state, and emits structured JSONL trace events (via the
//! shared `wisp::trace` module) so the `wisp-net-test` harness can verify
//! sync/replication/input behavior across N clients without a window or
//! rendering pipeline.
//!
//! Driven by an optional script file (env var `WISP_OBSERVER_SCRIPT`).
//! Lines are `<t_secs> <command> [args…]`; commands let the observer
//! simulate movement, throw lanterns, fire beams, place portals, etc.
//! Without a script the observer just sits idle at its spawn position and
//! reports whatever it receives.
//!
//! Example script:
//! ```text
//! 0.5  goto 0 1.5 -2
//! 1.0  throw_lantern 0 1.5 -2 0 3 -6
//! 2.0  exit
//! ```

use core::time::Duration;
use std::collections::VecDeque;
use std::net::SocketAddr;

use bevy::app::AppExit;
use bevy::log::LogPlugin;
use bevy::prelude::*;
use lightyear::netcode::prelude::Authentication;
use lightyear::prelude::client::{ClientPlugins, Connect, NetcodeClient, NetcodeConfig};
use lightyear::prelude::{Connected, LocalAddr, MessageSender, PeerAddr, ReplicationReceiver, UdpIo};
use serde_json::json;

use wisp::net::protocol::{
    BeamImpulseMessage, ParentCastInfo, PickupLanternMessage, PlacePortalMessage,
    PlayerInputChannel, PlayerInputMessage, PropShape, SpawnBodyMessage, ThrowLanternMessage,
};
use wisp::net::replication::ReplicationTracePlugin;
use wisp::net::{default_server_addr, ProtocolPlugin, NETCODE_KEY, PROTOCOL_ID, TICK_HZ};
use wisp::spells::portal::PortalSlot;
use wisp::trace;

fn main() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(LogPlugin::default());
    app.add_plugins(TransformPlugin);
    app.add_plugins(ClientPlugins {
        tick_duration: Duration::from_secs_f32(1.0 / TICK_HZ as f32),
    });
    app.add_plugins(ProtocolPlugin);
    app.add_plugins(ReplicationTracePlugin);
    app.add_plugins(trace::TracePlugin);

    app.insert_resource(ObserverConfig::from_env());
    app.insert_resource(ObserverState::default());
    app.insert_resource(ScriptedCommands::from_env());

    app.add_systems(Startup, spawn_observer_client);
    app.add_systems(
        Update,
        (
            advance_script,
            send_simulated_position,
        )
            .chain(),
    );
    app.add_observer(on_connected_observer);

    info!("wisp observer starting…");
    app.run();
}

/// Per-process configuration from env vars. Lets the harness spawn many
/// observers with distinct client ids without recompiling.
#[derive(Resource, Clone)]
struct ObserverConfig {
    server: SocketAddr,
    client_id: u64,
    spawn_pos: Vec3,
}

impl ObserverConfig {
    fn from_env() -> Self {
        let server = std::env::var("WISP_SERVER_ADDR")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(default_server_addr);
        let client_id = std::env::var("WISP_CLIENT_ID")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(pseudo_unique_client_id);
        let spawn_pos = std::env::var("WISP_SPAWN_POS")
            .ok()
            .and_then(parse_vec3)
            .unwrap_or(Vec3::new(0.0, 1.5, 0.0));
        Self {
            server,
            client_id,
            spawn_pos,
        }
    }
}

fn parse_vec3(s: String) -> Option<Vec3> {
    let parts: Vec<f32> = s.split(',').filter_map(|p| p.trim().parse().ok()).collect();
    if parts.len() == 3 {
        Some(Vec3::new(parts[0], parts[1], parts[2]))
    } else {
        None
    }
}

fn pseudo_unique_client_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// The observer's simulated body state. There's no real Player entity —
/// we just track position + yaw and broadcast `PlayerPositionMessage`
/// every Update so the server's per-client `NetworkedPlayer` updates and
/// other observers see us move.
#[derive(Resource, Default, Debug)]
struct ObserverState {
    pos: Vec3,
    yaw: f32,
    forward: Vec3,
    /// WASD axis vector sent in the per-tick `PlayerInputMessage`. Lets
    /// the observer actually drive the server-side player controller so
    /// the harness can verify server-authoritative movement, not just
    /// static-pose replication. Default zero = observer stands still.
    movement: Vec2,
    initialized: bool,
}

#[derive(Clone, Debug)]
enum Command {
    Goto { pos: Vec3 },
    Look { yaw: f32 },
    Forward { dir: Vec3 },
    MoveInput { axis: Vec2 },
    ThrowLantern { origin: Vec3, velocity: Vec3 },
    PickupLantern,
    Beam { origin: Vec3, direction: Vec3, range: f32, magnitude: f32 },
    PlacePortal { slot: PortalSlot, position: Vec3, normal: Vec3 },
    /// Send a `SpawnBodyMessage` shaped like the `fireball` body, with a
    /// `parent_cast` pointing at `fireball.throw` so the server attaches
    /// `BodyTriggers` (collision → explosion_small). Physics params are
    /// duplicated from `fireball.body.ron` — keep them in sync if the
    /// body file changes.
    ThrowFireball { origin: Vec3, velocity: Vec3 },
    Exit,
}

#[derive(Resource, Default)]
struct ScriptedCommands {
    queue: VecDeque<(f32, Command)>,
}

impl ScriptedCommands {
    fn from_env() -> Self {
        let path = match std::env::var("WISP_OBSERVER_SCRIPT").ok() {
            Some(p) if !p.is_empty() => p,
            _ => return Self::default(),
        };
        let body = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to read script {path}: {e}");
                return Self::default();
            }
        };
        let mut queue: Vec<(f32, Command)> = body.lines().filter_map(parse_line).collect();
        queue.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        info!("Observer script loaded with {} commands from {path}", queue.len());
        Self {
            queue: VecDeque::from(queue),
        }
    }
}

fn parse_line(line: &str) -> Option<(f32, Command)> {
    let line = line.split('#').next().unwrap_or("").trim();
    if line.is_empty() {
        return None;
    }
    let mut parts = line.split_whitespace();
    let t: f32 = parts.next()?.parse().ok()?;
    let kind = parts.next()?;
    let cmd = match kind {
        "goto" => Command::Goto {
            pos: read_vec3(&mut parts)?,
        },
        "look" => Command::Look {
            yaw: parts.next()?.parse().ok()?,
        },
        "forward" => Command::Forward {
            dir: read_vec3(&mut parts)?,
        },
        "move_input" => {
            let x: f32 = parts.next()?.parse().ok()?;
            let y: f32 = parts.next()?.parse().ok()?;
            Command::MoveInput {
                axis: Vec2::new(x, y),
            }
        }
        "throw_lantern" => {
            let origin = read_vec3(&mut parts)?;
            let velocity = read_vec3(&mut parts)?;
            Command::ThrowLantern { origin, velocity }
        }
        "pickup_lantern" => Command::PickupLantern,
        "beam" => {
            let origin = read_vec3(&mut parts)?;
            let direction = read_vec3(&mut parts)?;
            let range: f32 = parts.next()?.parse().ok()?;
            let magnitude: f32 = parts.next()?.parse().ok()?;
            Command::Beam {
                origin,
                direction,
                range,
                magnitude,
            }
        }
        "place_portal" => {
            let slot = match parts.next()? {
                "primary" => PortalSlot::Primary,
                "secondary" => PortalSlot::Secondary,
                _ => return None,
            };
            let position = read_vec3(&mut parts)?;
            let normal = read_vec3(&mut parts)?;
            Command::PlacePortal {
                slot,
                position,
                normal,
            }
        }
        "throw_fireball" => {
            let origin = read_vec3(&mut parts)?;
            let velocity = read_vec3(&mut parts)?;
            Command::ThrowFireball { origin, velocity }
        }
        "exit" => Command::Exit,
        _ => return None,
    };
    Some((t, cmd))
}

fn read_vec3<'a, I: Iterator<Item = &'a str>>(parts: &mut I) -> Option<Vec3> {
    let x: f32 = parts.next()?.parse().ok()?;
    let y: f32 = parts.next()?.parse().ok()?;
    let z: f32 = parts.next()?.parse().ok()?;
    Some(Vec3::new(x, y, z))
}

fn spawn_observer_client(mut commands: Commands, cfg: Res<ObserverConfig>) {
    let auth = Authentication::Manual {
        server_addr: cfg.server,
        client_id: cfg.client_id,
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
        "Observer connecting to {} as client_id={}…",
        cfg.server, cfg.client_id
    );
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    let entity = commands
        .spawn((
            client,
            UdpIo::default(),
            LocalAddr(local_addr),
            PeerAddr(cfg.server),
            ReplicationReceiver::default(),
        ))
        .id();
    commands.trigger(Connect { entity });
}

fn on_connected_observer(
    trigger: On<Add, Connected>,
    cfg: Res<ObserverConfig>,
    mut state: ResMut<ObserverState>,
    local_ids: Query<
        &lightyear::prelude::LocalId,
        Without<lightyear::prelude::server::ClientOf>,
    >,
) {
    if let Ok(lightyear::prelude::LocalId(peer_id)) = local_ids.get(trigger.entity) {
        info!("Observer connected: peer_id={peer_id:?}");
        trace::event("connected", json!({"peer_id": format!("{peer_id:?}")}));
    }
    state.pos = cfg.spawn_pos;
    state.yaw = 0.0;
    state.forward = Vec3::NEG_Z;
    state.initialized = true;
}

/// Each Update, broadcast our simulated body pose to the server. The
/// server uses it to drive our `NetworkedPlayer`, which replicates back
/// to every other connected observer/client.
fn send_simulated_position(
    state: Res<ObserverState>,
    sender: Option<Single<&mut MessageSender<PlayerInputMessage>>>,
) {
    if !state.initialized {
        return;
    }
    let Some(mut sender) = sender else { return };
    // Observers don't simulate physics or carry bei state — we send a
    // zero-input frame each tick so the server's authoritative
    // controller leaves the observer's player at its spawn position.
    // Script commands like `throw_lantern` carry their own
    // script-supplied origins, so they don't depend on the server's
    // notion of where we are.
    let _ = sender.send::<PlayerInputChannel>(PlayerInputMessage {
        movement: [state.movement.x, state.movement.y],
        yaw: state.yaw,
        jump: false,
        casting: false,
    });
}

/// Walks the scripted queue and fires due commands.
#[allow(clippy::too_many_arguments)]
fn advance_script(
    time: Res<Time>,
    mut script: ResMut<ScriptedCommands>,
    mut state: ResMut<ObserverState>,
    mut exit: MessageWriter<AppExit>,
    throw: Option<Single<&mut MessageSender<ThrowLanternMessage>>>,
    pickup: Option<Single<&mut MessageSender<PickupLanternMessage>>>,
    beam: Option<Single<&mut MessageSender<BeamImpulseMessage>>>,
    portal: Option<Single<&mut MessageSender<PlacePortalMessage>>>,
    spawn_body: Option<Single<&mut MessageSender<SpawnBodyMessage>>>,
) {
    let now = time.elapsed_secs();
    let mut throw = throw;
    let mut pickup = pickup;
    let mut beam = beam;
    let mut portal = portal;
    let mut spawn_body = spawn_body;
    while let Some((t, _)) = script.queue.front() {
        if *t > now {
            break;
        }
        let (t, cmd) = script.queue.pop_front().unwrap();
        match cmd {
            Command::Goto { pos } => {
                state.pos = pos;
                trace::event(
                    "script_goto",
                    json!({"t": t, "pos": [pos.x, pos.y, pos.z]}),
                );
            }
            Command::Look { yaw } => {
                state.yaw = yaw;
                state.forward = Quat::from_rotation_y(yaw) * Vec3::NEG_Z;
                trace::event("script_look", json!({"t": t, "yaw": yaw}));
            }
            Command::Forward { dir } => {
                state.pos += dir;
                trace::event(
                    "script_forward",
                    json!({"t": t, "delta": [dir.x, dir.y, dir.z]}),
                );
            }
            Command::MoveInput { axis } => {
                state.movement = axis;
                trace::event(
                    "script_move_input",
                    json!({"t": t, "axis": [axis.x, axis.y]}),
                );
            }
            Command::ThrowLantern { origin, velocity } => {
                if let Some(s) = throw.as_mut() {
                    let _ = s.send::<PlayerInputChannel>(ThrowLanternMessage {
                        origin: [origin.x, origin.y, origin.z],
                        velocity: [velocity.x, velocity.y, velocity.z],
                    });
                    trace::event(
                        "script_throw_lantern",
                        json!({
                            "t": t,
                            "origin": [origin.x, origin.y, origin.z],
                            "velocity": [velocity.x, velocity.y, velocity.z],
                        }),
                    );
                } else {
                    warn!("ThrowLantern requested but sender missing");
                }
            }
            Command::PickupLantern => {
                if let Some(s) = pickup.as_mut() {
                    let p = state.pos;
                    let _ = s.send::<PlayerInputChannel>(PickupLanternMessage {
                        player_position: [p.x, p.y, p.z],
                    });
                    trace::event("script_pickup_lantern", json!({"t": t}));
                }
            }
            Command::Beam {
                origin,
                direction,
                range,
                magnitude,
            } => {
                if let Some(s) = beam.as_mut() {
                    let _ = s.send::<PlayerInputChannel>(BeamImpulseMessage {
                        origin: [origin.x, origin.y, origin.z],
                        direction: [direction.x, direction.y, direction.z],
                        range,
                        magnitude,
                    });
                    trace::event(
                        "script_beam",
                        json!({
                            "t": t,
                            "origin": [origin.x, origin.y, origin.z],
                            "direction": [direction.x, direction.y, direction.z],
                            "magnitude": magnitude,
                        }),
                    );
                }
            }
            Command::PlacePortal {
                slot,
                position,
                normal,
            } => {
                if let Some(s) = portal.as_mut() {
                    let _ = s.send::<PlayerInputChannel>(PlacePortalMessage {
                        slot,
                        position: [position.x, position.y, position.z],
                        normal: [normal.x, normal.y, normal.z],
                    });
                    trace::event(
                        "script_place_portal",
                        json!({
                            "t": t,
                            "slot": format!("{slot:?}"),
                            "position": [position.x, position.y, position.z],
                            "normal": [normal.x, normal.y, normal.z],
                        }),
                    );
                }
            }
            Command::ThrowFireball { origin, velocity } => {
                if let Some(s) = spawn_body.as_mut() {
                    let msg = SpawnBodyMessage {
                        origin: [origin.x, origin.y, origin.z],
                        velocity: [velocity.x, velocity.y, velocity.z],
                        shape: PropShape::Sphere { radius: 0.15 },
                        mass: 0.4,
                        friction: 0.3,
                        linear_damping: 0.1,
                        angular_damping: 0.1,
                        restitution: 0.02,
                        tint_seed: 0.0,
                        parent_cast: Some(ParentCastInfo {
                            spell_id: "fireball".to_string(),
                            cast_id: "fireball.throw".to_string(),
                            captured_charge: Some(1.0),
                            chain_depth: 0,
                        }),
                    };
                    let _ = s.send::<PlayerInputChannel>(msg);
                    trace::event(
                        "script_throw_fireball",
                        json!({
                            "t": t,
                            "origin": [origin.x, origin.y, origin.z],
                            "velocity": [velocity.x, velocity.y, velocity.z],
                        }),
                    );
                }
            }
            Command::Exit => {
                trace::event("script_exit", json!({"t": t}));
                exit.write(AppExit::Success);
            }
        }
    }
}
