//! Identity-tag components used by data-driven spells. Each variant of
//! [`crate::spells::data::MarkerKind`] maps to one of these components. The
//! body spawner inserts them, conditions/effects query by them.

use bevy::prelude::*;

/// Placed lantern body. Acts as a beam source for the convex-lens family
/// of spells and as a `PortalTraveler` so it can fall through portals.
#[derive(Component)]
pub struct Lantern;
