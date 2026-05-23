//! Handler registry for `Delivery::Custom` / `Effect::Custom`. A spell
//! module registers a function under a named `HandlerId` during its plugin
//! build; the cast engine looks it up when a cast with that handler id runs.
//!
//! Handlers receive a `&mut World` (exclusive access) plus a [`CastContext`]
//! describing the player and the cast that's firing. Exclusive access keeps
//! handler implementations simple — no need to fight Bevy's parameter
//! scheduler for whatever queries the bespoke logic needs.

use std::collections::HashMap;
use std::sync::Arc;

use bevy::prelude::*;

use crate::spells::data::{CastId, HandlerId, OriginDef, SpellId, TargetDef};

/// Authority context. Filled in by the network layer; for single-player the
/// cast engine always passes `Authority::Both`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authority {
    /// Headless server: apply gameplay state, don't spawn local-only visuals.
    Server,
    /// Client predicting a locally-owned cast: spawn visuals, predict
    /// gameplay state.
    Client,
    /// Single-player or local listen-server: both apply.
    Both,
}

/// Context passed to a custom handler.
pub struct CastContext {
    pub player: Entity,
    pub spell_id: SpellId,
    pub cast_id: CastId,
    pub authority: Authority,
    /// For PressReleaseCharged casts, the charge captured at release. None
    /// for other trigger kinds and for charged casts that fired without
    /// reaching the release transition.
    pub captured_charge: Option<f32>,
    /// How deep in the parent → child cast chain this dispatch is.
    /// 0 for user-initiated casts; incremented by `dispatch_child_cast`
    /// when a body trigger fires. Plumbed into `SpawnBodyMessage` so
    /// server-side `BodyTriggers` know the parent's depth.
    pub chain_depth: u8,
    /// Effective origin for this dispatch — for child casts this is the
    /// override (e.g. `OriginDef::Impact(point)`), not the cast's authored
    /// origin. Handlers should read this rather than the catalog so the
    /// override is respected.
    pub origin: OriginDef,
    /// Cast's target spec, cloned for handler access. Stays as-authored
    /// today; child-cast dispatch does not override it.
    pub target: TargetDef,
}

pub type HandlerFn = dyn Fn(&mut World, &CastContext) + Send + Sync + 'static;

/// Registered set of named handlers. Populated during plugin build by each
/// spell module that uses `Delivery::Custom` or `Effect::Custom`.
#[derive(Resource, Default, Clone)]
pub struct HandlerRegistry {
    handlers: HashMap<HandlerId, Arc<HandlerFn>>,
}

impl HandlerRegistry {
    pub fn register<F>(&mut self, id: HandlerId, f: F)
    where
        F: Fn(&mut World, &CastContext) + Send + Sync + 'static,
    {
        if self.handlers.insert(id.clone(), Arc::new(f)).is_some() {
            warn!("Cast handler {:?} was re-registered; previous impl replaced.", id.0);
        }
    }

    pub fn get(&self, id: &HandlerId) -> Option<Arc<HandlerFn>> {
        self.handlers.get(id).cloned()
    }

    pub fn contains(&self, id: &HandlerId) -> bool {
        self.handlers.contains_key(id)
    }
}
