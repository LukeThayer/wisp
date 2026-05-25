use bevy::prelude::*;
use bevy_enhanced_input::prelude::*;

pub struct InputPlugin;

impl Plugin for InputPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(EnhancedInputPlugin)
            .add_input_context::<PlayerContext>()
            .add_input_context::<RadialMenuContext>()
            .init_resource::<InputMode>()
            .add_systems(Update, sync_contexts);
    }
}

#[derive(Component)]
pub struct PlayerContext;

#[derive(Component)]
pub struct RadialMenuContext;

#[derive(InputAction)]
#[action_output(Vec2)]
pub struct Movement;

#[derive(InputAction)]
#[action_output(Vec2)]
pub struct Look;

#[derive(InputAction)]
#[action_output(bool)]
pub struct Jump;

#[derive(InputAction)]
#[action_output(bool)]
pub struct OpenRadial;

/// Tap to cycle between equipped weapon slots (slot 0 ↔ slot 1).
/// Repopulates the radial wheel + snaps `ActiveSpell` to the new
/// weapon's first spell.
#[derive(InputAction)]
#[action_output(bool)]
pub struct SwapWeapon;

/// Tap to open / close the inventory modal where the player swaps
/// weapons into slots.
#[derive(InputAction)]
#[action_output(bool)]
pub struct OpenInventory;

#[derive(InputAction)]
#[action_output(bool)]
pub struct Fire;

#[derive(InputAction)]
#[action_output(bool)]
pub struct FireSecondary;

#[derive(InputAction)]
#[action_output(bool)]
pub struct ThrowLantern;

#[derive(InputAction)]
#[action_output(bool)]
pub struct Pickup;

/// Cycle through the local character roster (wizard, sorcerer, …) on each
/// press. Visual swap only — does not network-sync. Useful for sampling
/// the asset-pack characters in-game.
#[derive(InputAction)]
#[action_output(bool)]
pub struct CycleCharacter;

#[derive(Resource, Default, PartialEq, Eq, Clone, Copy, Debug)]
pub enum InputMode {
    #[default]
    Player,
    RadialMenu,
    /// Customization panel open: cursor is free, camera is 3rd-person,
    /// player input context is inactive so mouse-look + WASD are
    /// silently consumed by the UI.
    Customizing,
    /// Inventory modal open: cursor is free for clicking weapon /
    /// slot buttons, player input is silenced.
    Inventory,
}

/// `require_reset: true` keeps a held button from re-firing when the context
/// activates — opening the menu in Player ctx must not re-trigger Start in
/// RadialMenu ctx the next frame.
fn action<A: InputAction>() -> (Action<A>, ActionSettings) {
    (
        Action::<A>::new(),
        ActionSettings {
            require_reset: true,
            ..Default::default()
        },
    )
}

pub fn player_actions() -> impl Bundle {
    actions!(PlayerContext[
        (
            action::<Movement>(),
            DeadZone::default(),
            Bindings::spawn(Cardinal::wasd_keys()),
        ),
        (
            action::<Look>(),
            Bindings::spawn(Spawn((
                Binding::mouse_motion(),
                Negate::all(),
                Scale::splat(0.002),
            ))),
        ),
        (
            action::<Jump>(),
            bindings![KeyCode::Space, KeyCode::Enter],
        ),
        (
            action::<OpenRadial>(),
            bindings![KeyCode::KeyF],
        ),
        (
            action::<Fire>(),
            bindings![MouseButton::Left],
        ),
        (
            action::<FireSecondary>(),
            bindings![MouseButton::Right],
        ),
        (
            action::<ThrowLantern>(),
            bindings![KeyCode::KeyQ],
        ),
        (
            action::<Pickup>(),
            bindings![KeyCode::KeyE],
        ),
        (
            action::<CycleCharacter>(),
            bindings![KeyCode::KeyC],
        ),
        (
            action::<SwapWeapon>(),
            bindings![KeyCode::Tab],
        ),
        (
            action::<OpenInventory>(),
            bindings![KeyCode::KeyI],
        ),
    ])
}

pub fn radial_menu_actions() -> impl Bundle {
    // Empty: radial menu reads the cursor position directly from the window
    // and detects F release via raw ButtonInput, so it needs no bei actions.
    actions!(RadialMenuContext[])
}

fn sync_contexts(
    mode: Res<InputMode>,
    mut prev: Local<Option<InputMode>>,
    mut commands: Commands,
    q: Query<Entity, Or<(With<PlayerContext>, With<RadialMenuContext>)>>,
) {
    if *prev == Some(*mode) {
        return;
    }
    *prev = Some(*mode);

    let (player_active, menu_active) = match *mode {
        InputMode::Player => (true, false),
        InputMode::RadialMenu => (false, true),
        InputMode::Customizing => (false, false),
        InputMode::Inventory => (false, false),
    };

    for entity in &q {
        commands.entity(entity).insert((
            activity::<PlayerContext>(player_active),
            activity::<RadialMenuContext>(menu_active),
        ));
    }
}

fn activity<C: Component>(active: bool) -> ContextActivity<C> {
    if active {
        ContextActivity::<C>::ACTIVE
    } else {
        ContextActivity::<C>::INACTIVE
    }
}
