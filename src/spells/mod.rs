//! Spell framework. Three layers coexist during the migration:
//!
//! 1. **Data + asset pipeline** (`data`, `bodies`, `catalog`): RON files
//!    under `assets/spells/*.spell.ron` and `assets/bodies/*.body.ron` are
//!    loaded at startup and rebuilt on hot-reload into `SpellCatalog` /
//!    `BodyCatalog` resources.
//! 2. **Cast handlers** (`handlers`): bespoke Rust hooked behind named
//!    `Delivery::Custom` / `Effect::Custom` ids. Each spell module that
//!    needs custom behavior registers its handlers during plugin build.
//! 3. **Legacy per-spell registry** (this file's `SpellRegistry`): the
//!    activation/deactivation marker pattern from the first refactor. Still
//!    used by spells that haven't yet been migrated to the cast engine.
//!    Goes away once Lantern/ConvexLens/Iris/Portal are fully on the engine.
//!
//! Adding a new selectable spell (end state): drop a `.spell.ron` into
//! `assets/spells/`, plus any new body templates. No new Rust unless the
//! spell needs a custom handler.

use std::collections::HashMap;

use bevy::prelude::*;
use serde::Deserialize;

pub mod bodies;
pub mod catalog;
pub mod charge_orb;
pub mod convex_lens;
pub mod damage;
pub mod data;
pub mod deliveries;
pub mod effects;
pub mod engine;
pub mod handlers;
pub mod hud;
pub mod ice;
pub mod iris;
pub mod handheld_portal;
pub mod lantern;
pub mod markers;
pub mod explosion;
pub mod portal;
pub mod triggers;

pub use engine::{CastInstance, CastPhase, CastState, PrevActionSnapshot};
pub use handlers::HandlerRegistry;

pub struct SpellsPlugin;

impl Plugin for SpellsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SpellRegistry>()
            .init_resource::<HandlerRegistry>()
            .add_message::<SwitchSpell>()
            .add_plugins(catalog::CatalogPlugin)
            .add_plugins(engine::CastEnginePlugin)
            .add_plugins(hud::CastHudPlugin)
            .add_plugins(charge_orb::ChargeOrbPlugin)
            .add_systems(Update, apply_switch_spell)
            .add_systems(Update, engine::cancel_on_spell_change)
            .add_systems(Update, engine::add_action_snapshot_to_players);

        // Per-spell modules. Each one registers its plugin (observers /
        // systems / HUD overlays) and inserts an entry into the legacy
        // `SpellRegistry` (for `*Active` marker activation) plus a
        // `HandlerRegistry` entry if it uses `Delivery::Custom` /
        // `Effect::Custom`.
        //
        // Lantern is fully data-driven — its definition lives in
        // `assets/spells/lantern.spell.ron`. The radial menu reads labels
        // from `SpellCatalog` first, falling back to `SpellRegistry`, so
        // Lantern shows up in the menu without registering here.
        convex_lens::register(app);
        iris::register(app);
        portal::register(app);
        handheld_portal::register(app);
        lantern::register(app);
        ice::register(app);
    }
}

/// Identifier for a spell. A string newtype rather than an enum so spells
/// can be added purely as data (a new `.spell.ron` file ⇒ a new id) without
/// touching Rust. Used as a key in both the legacy [`SpellRegistry`] and the
/// asset-driven [`catalog::SpellCatalog`].
#[derive(Deserialize, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[serde(transparent)]
pub struct SpellId(pub String);

impl SpellId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
    /// Cheap comparison against a string literal (no allocation).
    pub fn is(&self, name: &str) -> bool {
        self.0 == name
    }
}

impl std::borrow::Borrow<str> for SpellId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SpellId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The currently-selected spell on the player. Mirrors whichever `*Active`
/// marker is on the player (for migrated spells: nothing — the cast engine
/// gates on this directly).
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub struct ActiveSpell(pub SpellId);

/// Buffered message emitted by the radial menu when the player picks a new
/// spell. Handled centrally by [`apply_switch_spell`].
#[derive(Message, Clone, Debug)]
pub struct SwitchSpell {
    pub player: Entity,
    pub spell: SpellId,
}

/// Legacy per-spell registry entry — keyed by `SpellId`. Holds the
/// activate/deactivate function pointers that flip the spell's `*Active`
/// marker component on the player.
pub struct SpellEntry {
    pub label: &'static str,
    pub activate: fn(&mut Commands, Entity),
    pub deactivate: fn(&mut Commands, Entity),
}

#[derive(Resource, Default)]
pub struct SpellRegistry {
    entries: HashMap<SpellId, SpellEntry>,
}

impl SpellRegistry {
    pub fn add(&mut self, id: SpellId, entry: SpellEntry) {
        self.entries.insert(id, entry);
    }

    pub fn get(&self, id: &SpellId) -> Option<&SpellEntry> {
        self.entries.get(id)
    }

    pub fn label(&self, id: &SpellId) -> &'static str {
        self.entries.get(id).map(|e| e.label).unwrap_or("Unknown")
    }
}

/// Handles `SwitchSpell` events by walking the registry: deactivate the
/// previously active spell's marker, activate the new one's marker, and
/// update the player's `ActiveSpell` component.
fn apply_switch_spell(
    mut events: MessageReader<SwitchSpell>,
    registry: Res<SpellRegistry>,
    mut commands: Commands,
    mut players: Query<&mut ActiveSpell>,
) {
    for ev in events.read() {
        let Ok(mut active) = players.get_mut(ev.player) else {
            continue;
        };
        if active.0 == ev.spell {
            continue;
        }
        if let Some(prev) = registry.get(&active.0) {
            (prev.deactivate)(&mut commands, ev.player);
        }
        if let Some(next) = registry.get(&ev.spell) {
            (next.activate)(&mut commands, ev.player);
        }
        active.0 = ev.spell.clone();
    }
}
