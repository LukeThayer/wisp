//! Wisp library: shared gameplay code used by both the `client` and
//! `server` binaries under `src/bin/`. Each binary composes its own plugin
//! set on top of [`build_shared`]:
//!
//! - Client adds `DefaultPlugins` (window, rendering, audio, input), egui
//!   inspector, and (Phase 2) `ClientNetPlugin`.
//! - Server adds `MinimalPlugins` (no rendering) and (Phase 2)
//!   `ServerNetPlugin`.
//!
//! `build_shared` registers the deterministic gameplay layer: physics,
//! input definitions, spells, player, world, and HUD. As multiplayer
//! support lands, some of these will be split into client-only /
//! server-only halves; for now they're shared.

use avian3d::prelude::*;
use bevy::prelude::*;

pub mod input;
pub mod net;
pub mod physics;
pub mod player;
pub mod spells;
pub mod trace;
pub mod ui;
pub mod weapons;
pub mod world;

/// Adds the gameplay plugin stack that both client and server need.
/// Intentionally does NOT add window / rendering / egui — bins layer those
/// in separately. Physics is added via [`add_avian_with_lightyear`] which
/// callers should invoke after their ClientPlugins/ServerPlugins.
pub fn build_shared(app: &mut App) {
    app.add_plugins((
        input::InputPlugin,
        weapons::WeaponsPlugin,
        spells::SpellsPlugin,
        player::PlayerPlugin,
        ui::UiPlugin,
        world::WorldPlugin,
    ));
}

/// Add avian physics with the lightyear integration plugin. Mirrors the
/// canonical setup from lightyear's `avian_3d_character` example
/// (`examples/avian_3d_character/src/shared.rs` at tag 0.26.4):
///
/// - `LightyearAvianPlugin` in `AvianReplicationMode::Position` so
///   replicated `Position` carries the authoritative pose and lightyear
///   owns the Transform ↔ Position sync (Stage Q).
/// - `PhysicsTransformPlugin` disabled — lightyear's plugin replaces it;
///   otherwise both would race to set Transform from Position and jitter.
/// - `PhysicsInterpolationPlugin` disabled — we use lightyear's
///   `add_linear_interpolation` registration on `Position`/`Rotation`.
/// - `IslandPlugin` + `IslandSleepingPlugin` disabled — sleeping bodies
///   misbehave under rollback (example example explicitly turns these
///   off).
pub fn add_avian_with_lightyear(app: &mut App) {
    app.add_plugins(lightyear::avian3d::plugin::LightyearAvianPlugin {
        replication_mode: lightyear::avian3d::plugin::AvianReplicationMode::Position,
        ..default()
    });
    app.add_plugins(
        PhysicsPlugins::default()
            .build()
            .disable::<PhysicsTransformPlugin>()
            .disable::<PhysicsInterpolationPlugin>()
            .disable::<IslandPlugin>()
            .disable::<IslandSleepingPlugin>(),
    );
}
