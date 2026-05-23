//! Catalog: loads `*.spell.ron` and `*.body.ron` from `assets/spells/` and
//! `assets/bodies/`, validates them, and exposes them as runtime resources.
//!
//! Pipeline:
//! 1. Startup kicks off `AssetServer::load_folder` for each directory.
//! 2. When the folder finishes loading (or a single asset changes for
//!    hot-reload), we walk the folder, pull each typed handle, and rebuild
//!    the catalog HashMap.
//! 3. Validation runs against the rebuilt catalog. If it fails we log the
//!    errors and **do not** replace the live catalog — a broken edit doesn't
//!    brick the running game.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use bevy::asset::LoadedFolder;
use bevy::prelude::*;
use bevy_common_assets::ron::RonAssetPlugin;

use crate::spells::bodies::BodyDef;
use crate::spells::data::{
    BodyTemplateId, CastDef, CastId, EffectDef, HandlerId, PayloadDef, SpellDef, SpellId,
};
use crate::spells::handlers::HandlerRegistry;

pub struct CatalogPlugin;

impl Plugin for CatalogPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(RonAssetPlugin::<SpellDef>::new(&["spell.ron"]))
            .add_plugins(RonAssetPlugin::<BodyDef>::new(&["body.ron"]))
            .init_resource::<SpellCatalog>()
            .init_resource::<BodyCatalog>()
            .init_resource::<CatalogFolders>()
            .add_systems(Startup, kick_off_folder_loads)
            .add_systems(Update, (rebuild_on_folder_event, rebuild_on_asset_event));
    }
}

/// Live spell catalog. Empty until the asset folders finish loading.
#[derive(Resource, Default)]
pub struct SpellCatalog {
    by_id: HashMap<SpellId, SpellDef>,
    order: Vec<SpellId>,
}

impl SpellCatalog {
    pub fn get(&self, id: &SpellId) -> Option<&SpellDef> {
        self.by_id.get(id)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&SpellId, &SpellDef)> {
        self.order.iter().filter_map(|id| self.by_id.get(id).map(|s| (id, s)))
    }
    pub fn ids(&self) -> impl Iterator<Item = &SpellId> {
        self.order.iter()
    }
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Test-only direct insert. The production catalog rebuilds itself
    /// from loaded RON; tests need to construct a catalog without the
    /// asset pipeline.
    #[cfg(test)]
    pub fn insert_for_test(&mut self, spell: SpellDef) {
        if !self.by_id.contains_key(&spell.id) {
            self.order.push(spell.id.clone());
        }
        self.by_id.insert(spell.id.clone(), spell);
    }
}

/// Live body-template catalog.
#[derive(Resource, Default)]
pub struct BodyCatalog {
    by_id: HashMap<BodyTemplateId, BodyDef>,
}

impl BodyCatalog {
    pub fn get(&self, id: &BodyTemplateId) -> Option<&BodyDef> {
        self.by_id.get(id)
    }
}

/// Folder handles we hold to keep the folder assets alive. Without this the
/// `LoadedFolder` (and the spell/body assets it tracks) gets dropped.
#[derive(Resource, Default)]
struct CatalogFolders {
    spells: Option<Handle<LoadedFolder>>,
    bodies: Option<Handle<LoadedFolder>>,
}

fn kick_off_folder_loads(
    asset_server: Res<AssetServer>,
    mut folders: ResMut<CatalogFolders>,
) {
    folders.spells = Some(asset_server.load_folder("spells"));
    folders.bodies = Some(asset_server.load_folder("bodies"));
}

/// Rebuilds the catalogs whenever a folder finishes loading (initial load
/// or a hot-reload swap).
fn rebuild_on_folder_event(
    mut folder_events: MessageReader<AssetEvent<LoadedFolder>>,
    folders: Res<CatalogFolders>,
    loaded_folders: Res<Assets<LoadedFolder>>,
    spell_assets: Res<Assets<SpellDef>>,
    body_assets: Res<Assets<BodyDef>>,
    handler_registry: Res<HandlerRegistry>,
    spell_catalog: ResMut<SpellCatalog>,
    body_catalog: ResMut<BodyCatalog>,
) {
    let mut dirty = false;
    for ev in folder_events.read() {
        match ev {
            AssetEvent::LoadedWithDependencies { .. }
            | AssetEvent::Added { .. }
            | AssetEvent::Modified { .. } => dirty = true,
            _ => {}
        }
    }
    if !dirty {
        return;
    }
    rebuild(
        &folders,
        &loaded_folders,
        &spell_assets,
        &body_assets,
        &handler_registry,
        spell_catalog,
        body_catalog,
    );
}

/// Rebuilds catalogs on individual asset hot-reloads so changes propagate
/// even when the folder itself isn't re-emitted.
fn rebuild_on_asset_event(
    mut spell_events: MessageReader<AssetEvent<SpellDef>>,
    mut body_events: MessageReader<AssetEvent<BodyDef>>,
    folders: Res<CatalogFolders>,
    loaded_folders: Res<Assets<LoadedFolder>>,
    spell_assets: Res<Assets<SpellDef>>,
    body_assets: Res<Assets<BodyDef>>,
    handler_registry: Res<HandlerRegistry>,
    spell_catalog: ResMut<SpellCatalog>,
    body_catalog: ResMut<BodyCatalog>,
) {
    let dirty_spell = spell_events.read().any(|e| {
        matches!(e, AssetEvent::Modified { .. } | AssetEvent::Added { .. })
    });
    let dirty_body = body_events.read().any(|e| {
        matches!(e, AssetEvent::Modified { .. } | AssetEvent::Added { .. })
    });
    if !dirty_spell && !dirty_body {
        return;
    }
    rebuild(
        &folders,
        &loaded_folders,
        &spell_assets,
        &body_assets,
        &handler_registry,
        spell_catalog,
        body_catalog,
    );
}

fn rebuild(
    folders: &CatalogFolders,
    loaded_folders: &Assets<LoadedFolder>,
    spell_assets: &Assets<SpellDef>,
    body_assets: &Assets<BodyDef>,
    handler_registry: &HandlerRegistry,
    mut spell_catalog: ResMut<SpellCatalog>,
    mut body_catalog: ResMut<BodyCatalog>,
) {
    let Some(body_folder) = folders.bodies.as_ref().and_then(|h| loaded_folders.get(h)) else {
        return;
    };
    let Some(spell_folder) = folders.spells.as_ref().and_then(|h| loaded_folders.get(h)) else {
        return;
    };

    // Pull every SpellDef + BodyDef the folders point at.
    let mut next_bodies: HashMap<BodyTemplateId, BodyDef> = HashMap::new();
    for handle in &body_folder.handles {
        if let Some(typed) = handle.clone().try_typed::<BodyDef>().ok() {
            if let Some(def) = body_assets.get(&typed) {
                next_bodies.insert(def.id.clone(), def.clone());
            }
        }
    }

    let mut next_spells: HashMap<SpellId, SpellDef> = HashMap::new();
    let mut next_order: Vec<SpellId> = Vec::new();
    for handle in &spell_folder.handles {
        if let Some(typed) = handle.clone().try_typed::<SpellDef>().ok() {
            if let Some(def) = spell_assets.get(&typed) {
                if !next_spells.contains_key(&def.id) {
                    next_order.push(def.id.clone());
                }
                next_spells.insert(def.id.clone(), def.clone());
            }
        }
    }
    // Stable order across reloads.
    next_order.sort();

    // Validate against the candidate catalog. If invalid, refuse the swap.
    if let Err(errors) = validate(&next_spells, &next_bodies, handler_registry) {
        warn!(
            "Spell catalog validation failed ({} error{}); keeping previous catalog live.",
            errors.len(),
            if errors.len() == 1 { "" } else { "s" },
        );
        for err in errors {
            warn!("  - {err}");
        }
        return;
    }

    // Only log when something actually changed. Rebuild fires for every
    // individual asset event during startup (12+ assets → 12+ rebuilds),
    // so logging unconditionally floods the log. We catch:
    //   - transition from empty (first successful load), and
    //   - id-set deltas (a spell or body added/removed at hot-reload).
    // Body-only edits (mesh tweak, physics tune) don't change ids and
    // stay quiet — drop to RUST_LOG=debug if you need to see them.
    let was_empty = spell_catalog.is_empty();
    let spell_ids_changed = next_order != spell_catalog.order;
    let body_ids_changed: HashSet<&BodyTemplateId> = next_bodies.keys().collect();
    let prev_body_ids: HashSet<&BodyTemplateId> = body_catalog.by_id.keys().collect();
    let body_set_changed = body_ids_changed != prev_body_ids;
    if was_empty || spell_ids_changed || body_set_changed {
        info!(
            "Spell catalog: {} spell{}, {} bod{}.",
            next_spells.len(),
            if next_spells.len() == 1 { "" } else { "s" },
            next_bodies.len(),
            if next_bodies.len() == 1 { "y" } else { "ies" },
        );
    }

    spell_catalog.by_id = next_spells;
    spell_catalog.order = next_order;
    body_catalog.by_id = next_bodies;
}

/// Validate referential integrity of the candidate catalog. Returns the set
/// of problems found; an empty Ok means the catalog is safe to make live.
fn validate(
    spells: &HashMap<SpellId, SpellDef>,
    bodies: &HashMap<BodyTemplateId, BodyDef>,
    handlers: &HandlerRegistry,
) -> Result<(), Vec<String>> {
    let mut errors: Vec<String> = Vec::new();

    let mut seen_cast_ids: HashSet<CastId> = HashSet::new();
    for (spell_id, spell) in spells {
        if &spell.id != spell_id {
            errors.push(format!(
                "spell {:?}: file id {:?} doesn't match map key",
                spell_id.0, spell.id.0
            ));
        }
        for cast in &spell.casts {
            if !seen_cast_ids.insert(cast.id.clone()) {
                errors.push(format!("duplicate cast id {:?}", cast.id.0));
            }
            validate_cast(cast, bodies, handlers, &mut errors);
        }
    }

    for (id, body) in bodies {
        if &body.id != id {
            errors.push(format!(
                "body {:?}: file id {:?} doesn't match map key",
                id.0, body.id.0
            ));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_cast(
    cast: &CastDef,
    bodies: &HashMap<BodyTemplateId, BodyDef>,
    handlers: &HandlerRegistry,
    errors: &mut Vec<String>,
) {
    if cast.cooldown < 0.0 {
        errors.push(format!(
            "cast {:?}: negative cooldown {}",
            cast.id.0, cast.cooldown
        ));
    }
    match &cast.payload {
        PayloadDef::SpawnBody { template, .. } => {
            if !bodies.contains_key(template) {
                errors.push(format!(
                    "cast {:?}: references unknown body template {:?}",
                    cast.id.0, template.0
                ));
            }
        }
        PayloadDef::Custom { handler } => warn_handler(handler, &cast.id.0, handlers),
        _ => {}
    }
    if let EffectDef::Custom { handler } = &cast.effect {
        warn_handler(handler, &cast.id.0, handlers);
    }
}

/// Missing handlers warn but don't block catalog load. A peer that
/// doesn't register a given handler (e.g. server doesn't have client-only
/// visual handlers) still loads the spell — runtime dispatch warns if it
/// actually tries to call the missing handler. Body templates remain a
/// hard error because they're authoring-time references that can't be
/// gracefully ignored.
///
/// Catalog rebuild fires for every individual asset event during startup
/// (12+ assets → 12+ rebuilds), so we dedupe via a process-wide seen set:
/// each (cast, handler) pair warns exactly once per process. Re-registering
/// a handler after a warn won't re-warn — that's fine, the next rebuild
/// will validate cleanly and the next dispatch wouldn't have warned anyway.
fn warn_handler(handler: &HandlerId, cast_id: &str, handlers: &HandlerRegistry) {
    if handlers.contains(handler) {
        return;
    }
    static SEEN: Mutex<Option<HashSet<(String, String)>>> = Mutex::new(None);
    let mut guard = SEEN.lock().unwrap();
    let seen = guard.get_or_insert_with(HashSet::new);
    if !seen.insert((cast_id.to_string(), handler.0.clone())) {
        return;
    }
    warn!(
        "Cast {:?} references handler {:?} not registered on this peer; \
         dispatch will no-op if this peer tries to run the cast.",
        cast_id, handler.0
    );
}
