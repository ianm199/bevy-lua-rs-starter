//! bevy-lua-rs-starter
//!
//! Bevy 0.18 + lua-rs 0.0.9, runs natively and on wasm. Demonstrates the
//! ECS-direct scripting shape requested in ianm199/lua-rs#23.
//!
//! Architecture:
//! - Components (`Position`, `Velocity`, `LuaBehavior`) are real Bevy `Component`
//!   types that derive `LuaUserData`. The derive generates field get/set and
//!   `#[lua_methods]` exposes inherent methods like `Velocity:magnitude()`.
//! - One `Lua` per app, stored as a `NonSend` resource (Lua is `Rc<LuaInner>`).
//! - Multiple Lua scripts loaded into the same VM at startup. Each defines its
//!   own globals/helpers; one shared `behaviors` table holds named per-entity
//!   behaviors keyed by string.
//! - A `world` global gives scripts `spawn`/`each`/`get`/`set`/`despawn`. While
//!   a Bevy system is calling into Lua, the world pointer is stashed in a
//!   thread-local so the world methods can borrow it back without trying to
//!   thread `&mut World` through Lua values.
//! - Each frame Rust iterates entities that have a `LuaBehavior` component and
//!   calls `behaviors[name](entity, dt)` for each. That is the per-entity
//!   scripting model (one VM, many scripts, each entity opts in to a behavior).
//! - Bevy renders each entity as a coloured sprite; a sync system mirrors
//!   `Position` into `Transform` so the simulation logic stays in plain Lua
//!   coordinates and the renderer doesn't care.

use std::cell::RefCell;

use bevy::asset::AssetMetaCheck;
use bevy::color::Color;
use bevy::prelude::*;

use lua_rs_runtime::{
    lua_methods, AnyUserData, FromLua, Function, IntoLua, Lua, LuaError, LuaUserData, MetaMethod,
    Result as LuaResult, Table, UserData, UserDataMethods, Value, Variadic,
};

// Two scripts compiled into the binary so the demo runs identically on native
// and wasm without an asset server. A real game would load these from disk.
const SCRIPT_BEHAVIORS: &str = include_str!("../assets/scripts/behaviors.lua");
const SCRIPT_INIT: &str = include_str!("../assets/scripts/init.lua");

// ============================================================================
// Components
// ============================================================================

#[derive(Component, Clone, Debug, LuaUserData)]
#[lua_impl(Display)]
pub struct Position {
    pub x: f64,
    pub y: f64,
}

impl std::fmt::Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Position({:.2}, {:.2})", self.x, self.y)
    }
}

#[derive(Component, Clone, Debug, LuaUserData)]
#[lua(methods)]
pub struct Velocity {
    pub x: f64,
    pub y: f64,
}

#[lua_methods]
impl Velocity {
    pub fn magnitude(&self) -> f64 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
}

/// Per-entity script binding: names a function inside Lua's `behaviors` table.
#[derive(Component, Clone, Debug, LuaUserData)]
pub struct LuaBehavior {
    pub name: String,
}

// ============================================================================
// EntityHandle (Lua-side)
// ============================================================================

#[derive(Clone, Copy, Debug)]
pub struct EntityHandle(pub Entity);

impl UserData for EntityHandle {
    fn add_meta_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!("Entity({:?})", this.0))
        });
        m.add_meta_method(MetaMethod::Eq, |_, this, other: Value| {
            if let Value::UserData(ud) = other {
                if let Ok(o) = ud.borrow::<EntityHandle>() {
                    return Ok(this.0 == o.0);
                }
            }
            Ok(false)
        });
    }
}

impl FromLua for EntityHandle {
    fn from_lua(value: Value, _lua: &Lua) -> LuaResult<Self> {
        match value {
            Value::UserData(ud) => {
                let borrowed = ud
                    .borrow::<EntityHandle>()
                    .map_err(|_| LuaError::runtime(format_args!("expected an EntityHandle")))?;
                Ok(*borrowed)
            }
            other => Err(LuaError::runtime(format_args!(
                "expected an EntityHandle, got {other:?}"
            ))),
        }
    }
}

// ============================================================================
// World access via a thread-local pointer
// ============================================================================

thread_local! {
    static WORLD_CELL: RefCell<Option<*mut World>> = const { RefCell::new(None) };
}

fn with_world<R>(world: &mut World, f: impl FnOnce() -> R) -> R {
    WORLD_CELL.with(|c| *c.borrow_mut() = Some(world as *mut World));
    let out = f();
    WORLD_CELL.with(|c| *c.borrow_mut() = None);
    out
}

fn with_world_mut<R>(f: impl FnOnce(&mut World) -> LuaResult<R>) -> LuaResult<R> {
    WORLD_CELL.with(|c| match *c.borrow() {
        // SAFETY: pointer is only set inside `with_world`, which holds an
        // exclusive borrow on the `World` for the duration of `f`. Lua callbacks
        // run synchronously inside that borrow.
        Some(ptr) => f(unsafe { &mut *ptr }),
        None => Err(LuaError::runtime(format_args!(
            "world is only accessible inside on_load / behaviors / world:each callbacks"
        ))),
    })
}

// ============================================================================
// `world` global
// ============================================================================

#[derive(Clone, Copy)]
struct WorldProxy;

fn try_insert_component(entity: &mut EntityWorldMut, ud: &AnyUserData) -> LuaResult<()> {
    if let Ok(c) = ud.borrow::<Position>() {
        entity.insert(c.clone());
        return Ok(());
    }
    if let Ok(c) = ud.borrow::<Velocity>() {
        entity.insert(c.clone());
        return Ok(());
    }
    if let Ok(c) = ud.borrow::<LuaBehavior>() {
        entity.insert(c.clone());
        return Ok(());
    }
    Err(LuaError::runtime(format_args!(
        "world:spawn got a userdata that is not a registered component"
    )))
}

fn query_entries(world: &mut World, lua: &Lua, name: &str) -> LuaResult<Vec<(Entity, Value)>> {
    match name {
        "Position" => {
            let mut q = world.query::<(Entity, &Position)>();
            let mut out = Vec::new();
            for (e, c) in q.iter(world) {
                out.push((e, c.clone().into_lua(lua)?));
            }
            Ok(out)
        }
        "Velocity" => {
            let mut q = world.query::<(Entity, &Velocity)>();
            let mut out = Vec::new();
            for (e, c) in q.iter(world) {
                out.push((e, c.clone().into_lua(lua)?));
            }
            Ok(out)
        }
        "LuaBehavior" => {
            let mut q = world.query::<(Entity, &LuaBehavior)>();
            let mut out = Vec::new();
            for (e, c) in q.iter(world) {
                out.push((e, c.clone().into_lua(lua)?));
            }
            Ok(out)
        }
        other => Err(LuaError::runtime(format_args!(
            "unknown component {other:?}"
        ))),
    }
}

fn get_component(world: &World, entity: Entity, name: &str, lua: &Lua) -> LuaResult<Value> {
    let v = match name {
        "Position" => world.get::<Position>(entity).cloned().map(|c| c.into_lua(lua)),
        "Velocity" => world.get::<Velocity>(entity).cloned().map(|c| c.into_lua(lua)),
        "LuaBehavior" => world.get::<LuaBehavior>(entity).cloned().map(|c| c.into_lua(lua)),
        other => {
            return Err(LuaError::runtime(format_args!(
                "unknown component {other:?}"
            )));
        }
    };
    Ok(v.transpose()?.unwrap_or(Value::Nil))
}

fn set_component(world: &mut World, entity: Entity, name: &str, value: Value) -> LuaResult<()> {
    let Value::UserData(ud) = value else {
        return Err(LuaError::runtime(format_args!(
            "world:set value must be component userdata"
        )));
    };
    let Ok(mut e) = world.get_entity_mut(entity) else {
        return Err(LuaError::runtime(format_args!(
            "entity {entity:?} does not exist"
        )));
    };
    match name {
        "Position" => {
            let c = ud.borrow::<Position>().map_err(|_| {
                LuaError::runtime(format_args!("not a Position userdata"))
            })?;
            e.insert(c.clone());
        }
        "Velocity" => {
            let c = ud.borrow::<Velocity>().map_err(|_| {
                LuaError::runtime(format_args!("not a Velocity userdata"))
            })?;
            e.insert(c.clone());
        }
        "LuaBehavior" => {
            let c = ud.borrow::<LuaBehavior>().map_err(|_| {
                LuaError::runtime(format_args!("not a LuaBehavior userdata"))
            })?;
            e.insert(c.clone());
        }
        other => {
            return Err(LuaError::runtime(format_args!(
                "unknown component {other:?}"
            )));
        }
    }
    Ok(())
}

impl UserData for WorldProxy {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("spawn", |_lua, _this, args: Variadic<Value>| {
            with_world_mut(|world| {
                let mut entity = world.spawn_empty();
                for value in args.into_iter() {
                    match value {
                        Value::UserData(ud) => try_insert_component(&mut entity, &ud)?,
                        other => {
                            return Err(LuaError::runtime(format_args!(
                                "world:spawn expects component userdata, got {other:?}"
                            )));
                        }
                    }
                }
                Ok(EntityHandle(entity.id()))
            })
        });

        m.add_method(
            "each",
            |lua, _this, (name, callback): (String, Function)| {
                let pairs = with_world_mut(|world| query_entries(world, lua, &name))?;
                for (entity, value) in pairs {
                    let _: Value = callback.call((EntityHandle(entity), value))?;
                }
                Ok(())
            },
        );

        m.add_method(
            "get",
            |lua, _this, (entity, name): (EntityHandle, String)| {
                with_world_mut(|world| get_component(world, entity.0, &name, lua))
            },
        );

        m.add_method(
            "set",
            |_lua, _this, (entity, name, value): (EntityHandle, String, Value)| {
                with_world_mut(|world| set_component(world, entity.0, &name, value))
            },
        );

        m.add_method("despawn", |_lua, _this, entity: EntityHandle| {
            with_world_mut(|world| {
                world.despawn(entity.0);
                Ok(())
            })
        });
    }
}

// ============================================================================
// Plugin
// ============================================================================

struct LuaRuntime {
    lua: Lua,
}

pub struct LuaScriptingPlugin;

impl Plugin for LuaScriptingPlugin {
    fn build(&self, app: &mut App) {
        let lua = Lua::new();
        install_globals(&lua).expect("install lua globals");
        app.insert_non_send_resource(LuaRuntime { lua });
        app.add_systems(Startup, load_scripts);
        app.add_systems(Update, (ensure_visuals, tick_behaviors, sync_transforms).chain());
    }
}

fn install_globals(lua: &Lua) -> LuaResult<()> {
    let globals = lua.globals();
    globals.set(
        "Position",
        lua.create_function(|_, (x, y): (f64, f64)| Ok(Position { x, y }))?,
    )?;
    globals.set(
        "Velocity",
        lua.create_function(|_, (x, y): (f64, f64)| Ok(Velocity { x, y }))?,
    )?;
    globals.set(
        "LuaBehavior",
        lua.create_function(|_, name: String| Ok(LuaBehavior { name }))?,
    )?;
    let world = lua.create_userdata(WorldProxy)?;
    globals.set("world", world)?;
    Ok(())
}

/// Loads every script into the same VM at startup, in order, then calls
/// `on_load()`. The world pointer is set for the duration so `world:spawn`
/// inside `on_load` works.
fn load_scripts(world: &mut World) {
    with_world(world, || {
        let lua = world_lua();
        for (name, src) in [
            ("behaviors.lua", SCRIPT_BEHAVIORS),
            ("init.lua", SCRIPT_INIT),
        ] {
            if let Err(e) = lua.load(src).set_name(name.as_bytes()).exec() {
                error!("loading {name}: {e}");
            }
        }
        if let Ok(on_load) = lua.globals().get::<_, Function>("on_load") {
            if let Err(e) = on_load.call::<_, Value>(()) {
                error!("on_load: {e}");
            }
        }
    });
}

/// Look up each entity's `LuaBehavior` and call `behaviors[name](entity, dt)`.
/// Entities are collected outside the `with_world` so we don't hold a query
/// borrow across Lua reentry.
fn tick_behaviors(world: &mut World) {
    let dt = world.resource::<Time>().delta_secs() as f64;
    let pairs: Vec<(Entity, String)> = {
        let mut q = world.query::<(Entity, &LuaBehavior)>();
        q.iter(world).map(|(e, b)| (e, b.name.clone())).collect()
    };
    if pairs.is_empty() {
        return;
    }
    let lua = world.non_send_resource::<LuaRuntime>().lua.clone();
    with_world(world, || {
        let Ok(behaviors) = lua.globals().get::<_, Table>("behaviors") else {
            return;
        };
        for (entity, name) in pairs {
            let Ok(func) = behaviors.get::<_, Function>(name.as_str()) else {
                continue;
            };
            if let Err(e) = func.call::<_, Value>((EntityHandle(entity), dt)) {
                error!("behavior '{name}' on {entity:?}: {e}");
            }
        }
    });
}

fn world_lua() -> Lua {
    WORLD_CELL.with(|c| {
        let ptr = c
            .borrow()
            .expect("world_lua called outside with_world");
        // SAFETY: same invariant as `with_world_mut`.
        let world: &mut World = unsafe { &mut *ptr };
        world.non_send_resource::<LuaRuntime>().lua.clone()
    })
}

// ============================================================================
// Rendering
// ============================================================================

/// Attach a `Sprite` + `Transform` to any entity that has a `Position` but no
/// `Sprite` yet. Colour is keyed off the behaviour name so each kind reads at
/// a glance.
fn ensure_visuals(
    mut commands: Commands,
    query: Query<(Entity, &Position, Option<&LuaBehavior>), Without<Sprite>>,
) {
    for (entity, pos, behavior) in &query {
        let color = match behavior.map(|b| b.name.as_str()) {
            Some("wander") => Color::srgb(0.95, 0.45, 0.30),
            Some("orbit") => Color::srgb(0.30, 0.75, 0.95),
            Some("bounce") => Color::srgb(0.45, 0.95, 0.50),
            _ => Color::srgb(0.80, 0.80, 0.80),
        };
        commands.entity(entity).insert((
            Sprite::from_color(color, Vec2::splat(18.0)),
            Transform::from_xyz(pos.x as f32, pos.y as f32, 0.0),
        ));
    }
}

fn sync_transforms(mut q: Query<(&Position, &mut Transform)>) {
    for (pos, mut t) in &mut q {
        t.translation.x = pos.x as f32;
        t.translation.y = pos.y as f32;
    }
}

fn setup(mut commands: Commands) {
    commands.spawn(Camera2d);
}

// ============================================================================
// `main`
// ============================================================================

fn main() {
    #[cfg(target_arch = "wasm32")]
    console_error_panic_hook::set_once();

    App::new()
        .add_plugins(
            DefaultPlugins
                .set(AssetPlugin {
                    meta_check: AssetMetaCheck::Never,
                    ..default()
                })
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        title: "bevy-lua-rs-starter".into(),
                        canvas: Some("#bevy".into()),
                        fit_canvas_to_parent: true,
                        ..default()
                    }),
                    ..default()
                }),
        )
        .add_plugins(LuaScriptingPlugin)
        .add_systems(Startup, setup)
        .insert_resource(ClearColor(Color::srgb(0.08, 0.09, 0.14)))
        .run();
}
