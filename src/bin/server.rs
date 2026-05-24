//! Server binary. Headless — runs only the network layer for now. Gameplay
//! state replication (player, lanterns, portals, props) lands in Stage N+;
//! until then this just listens, accepts connections, and logs.

use bevy::log::LogPlugin;
use bevy::mesh::MeshPlugin;
use bevy::prelude::*;
use bevy::scene::ScenePlugin;

fn main() {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        LogPlugin::default(),
        AssetPlugin::default(),
        TransformPlugin,
        // Avian's collider cache reads `AssetEvent<Mesh>` and looks up
        // `SceneSpawner`; without these asset registrations, system
        // param validation panics at startup. We don't render anything,
        // but the type registrations are required.
        MeshPlugin,
        ScenePlugin,
    ));
    app.add_plugins(wisp::net::ServerNetPlugin);
    // Physics goes after ServerPlugins so LightyearAvianPlugin sees the
    // replication infra already initialised (Stage Q).
    wisp::add_avian_with_lightyear(&mut app);
    // Spell catalog: needed so `handle_spawn_body` can resolve
    // `parent_cast` info on `SpawnBodyMessage` and look up `on_event`
    // hooks. The server doesn't run the cast engine itself — only the
    // catalog + handler registry + body-trigger plugin.
    app.init_resource::<wisp::spells::HandlerRegistry>();
    app.add_plugins(wisp::spells::catalog::CatalogPlugin);
    app.add_plugins(wisp::spells::triggers::BodyTriggersPlugin);
    app.add_plugins(wisp::spells::explosion::ExplosionPlugin);
    // DamagePlugin: hurtbox/hitbox contact damage system + DeathEvent
    // routing (NetworkedPlayer -> respawn, anything else -> despawn).
    // Required so AreaDamage effects on spells (e.g. explosion_small)
    // actually apply hp changes.
    app.add_plugins(wisp::spells::damage::DamagePlugin);
    info!("wisp server starting…");
    app.run();
}
