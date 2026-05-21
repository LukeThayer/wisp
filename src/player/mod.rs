pub mod controller;
pub mod spells;

use bevy::prelude::*;

pub struct PlayerPlugin;

impl Plugin for PlayerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<spells::ActiveSpell>()
            .add_observer(controller::apply_look)
            .add_observer(controller::apply_movement)
            .add_observer(controller::apply_jump)
            .add_systems(
                Update,
                (
                    controller::apply_rotation,
                    controller::cursor_grab,
                    controller::apply_ground_brake,
                ),
            )
            .add_systems(FixedUpdate, controller::ground_check);
    }
}

pub use controller::{Facing, Player, PlayerCamera};
