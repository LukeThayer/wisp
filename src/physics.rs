use avian3d::prelude::*;

/// Collision layers shared across the project. Lives at the crate root so
/// world spawn, the player controller, and the portal spell can all agree on
/// who collides with what without one spell module owning the definition.
///
/// The interesting case is `Player` vs `Ground`: a player straddling a
/// horizontal portal disc has `Ground` temporarily dropped from its filter so
/// gravity pulls them through the floor; see `spells::portal`.
#[derive(PhysicsLayer, Clone, Copy, Debug, Default)]
pub enum GameLayer {
    #[default]
    Default,
    Player,
    Ground,
}
