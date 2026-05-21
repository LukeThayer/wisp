use bevy::prelude::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum SpellId {
    #[default]
    Empty,
    ConvexLens,
    Iris,
    Portal,
    Placeholder4,
    Placeholder5,
    Placeholder6,
    Placeholder7,
    Placeholder8,
}

impl SpellId {
    pub fn label(self) -> &'static str {
        match self {
            SpellId::Empty => "Empty",
            SpellId::ConvexLens => "Convex Lens",
            SpellId::Iris => "Iris",
            SpellId::Portal => "Portal",
            SpellId::Placeholder4 => "Slot 4",
            SpellId::Placeholder5 => "Slot 5",
            SpellId::Placeholder6 => "Slot 6",
            SpellId::Placeholder7 => "Slot 7",
            SpellId::Placeholder8 => "Slot 8",
        }
    }
}

#[derive(Component)]
pub struct EquippedSpells(pub [SpellId; 8]);

impl Default for EquippedSpells {
    fn default() -> Self {
        Self([
            SpellId::ConvexLens,
            SpellId::Iris,
            SpellId::Portal,
            SpellId::Placeholder4,
            SpellId::Placeholder5,
            SpellId::Placeholder6,
            SpellId::Placeholder7,
            SpellId::Placeholder8,
        ])
    }
}

#[derive(Resource)]
pub struct ActiveSpell(pub SpellId);

impl Default for ActiveSpell {
    fn default() -> Self {
        Self(SpellId::ConvexLens)
    }
}
