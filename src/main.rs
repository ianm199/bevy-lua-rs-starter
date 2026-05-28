//! bevy-lua-rs-starter
//!
//! A minimal Bevy + lua-rs integration that demonstrates the ECS-direct
//! scripting shape requested in <https://github.com/ianm199/lua-rs/issues/23>.
//!
//! Design choices (each is deliberate):
//! - One language (Lua), one runtime per app. No `bevy_mod_scripting` abstraction.
//! - Components are real Bevy `Component` types that derive `LuaUserData`. They
//!   appear in Lua as userdata; the derive generates field get/set and the
//!   `#[lua_methods]` attribute exposes inherent methods as `obj:method(...)`.
//! - A `world` global gives scripts `spawn` / `each` / `get` / `set` / `despawn`.
//!   Inside a Lua callback, the world pointer is borrowed via a thread-local
//!   set by the Bevy system that called into Lua, so callbacks can mutate the
//!   ECS safely without trying to thread a `&mut World` through Lua values.
//! - Scripts hot-reload by polling the file's mtime. On reload the script is
//!   re-executed and its `on_load()` runs again; `on_update(dt)` runs every
//!   frame.

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;

use lua_rs_runtime::{
    lua_methods, AnyUserData, FromLua, Function, IntoLua, Lua, LuaError, LuaUserData, MetaMethod,
    Result as LuaResult, UserData, UserDataMethods, Value, Variadic,
};

// ============================================================================
// Components
// ============================================================================

/// A 2D position. Fields are exposed to Lua as `pos.x` / `pos.y`; `tostring(pos)`
/// uses the `Display` impl wired in via `#[lua_impl(Display)]`.
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

/// A 2D velocity with a `magnitude()` method exposed to Lua.
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

// ============================================================================
// Entity handle for Lua scripts
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
//
// Bevy systems take exclusive access to `&mut World`. Lua callbacks are
// `Fn`-typed and can be invoked re-entrantly from inside a system. We stash
// the world pointer in a thread-local for the duration of one system tick;
// Lua-side world methods (`world:spawn`, `world:each`, ...) re-borrow it.

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
        // SAFETY: the pointer is only set inside `with_world`, which holds an
        // exclusive borrow on the `World` for the duration of `f`. Lua-side
        // callbacks run synchronously inside that borrow.
        Some(ptr) => f(unsafe { &mut *ptr }),
        None => Err(LuaError::runtime(format_args!(
            "world is only accessible inside on_load / on_update / world:each callbacks"
        ))),
    })
}

// ============================================================================
// The `world` global
// ============================================================================
//
// Adding a new component is a one-liner each in `try_insert_component`,
// `query_entries`, `get_component`, and `set_component`. A macro could collapse
// these but the explicit table here is exactly what Shatur asked for in #23:
// type-safe Rust bindings, one language, no reflect magic.

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
        other => Err(LuaError::runtime(format_args!(
            "unknown component {other:?}; expected Position or Velocity"
        ))),
    }
}

fn get_component(world: &World, entity: Entity, name: &str, lua: &Lua) -> LuaResult<Value> {
    let v = match name {
        "Position" => world.get::<Position>(entity).cloned().map(|c| c.into_lua(lua)),
        "Velocity" => world.get::<Velocity>(entity).cloned().map(|c| c.into_lua(lua)),
        other => {
            return Err(LuaError::runtime(format_args!(
                "unknown component {other:?}"
            )));
        }
    };
    Ok(v.transpose()?.unwrap_or(Value::Nil))
}

fn set_component(
    world: &mut World,
    entity: Entity,
    name: &str,
    value: Value,
) -> LuaResult<()> {
    let Value::UserData(ud) = value else {
        return Err(LuaError::runtime(format_args!(
            "world:set value must be a component userdata"
        )));
    };
    let Ok(mut e) = world.get_entity_mut(entity) else {
        return Err(LuaError::runtime(format_args!(
            "entity {entity:?} does not exist"
        )));
    };
    match name {
        "Position" => {
            let c = ud
                .borrow::<Position>()
                .map_err(|_| LuaError::runtime(format_args!("not a Position userdata")))?;
            e.insert(c.clone());
        }
        "Velocity" => {
            let c = ud
                .borrow::<Velocity>()
                .map_err(|_| LuaError::runtime(format_args!("not a Velocity userdata")))?;
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
// The Bevy plugin
// ============================================================================

/// Lua is `Rc<LuaInner>` and so is `!Send + !Sync`. Bevy expects `Resource` to be
/// both, so the runtime goes in as a *non-send* resource. That keeps it on the
/// main thread, which is exactly what a single-threaded VM wants.
struct LuaRuntime {
    lua: Lua,
}

#[derive(Resource)]
struct ScriptFile {
    path: PathBuf,
    last_modified: Option<SystemTime>,
    loaded: bool,
}

pub struct LuaScriptingPlugin {
    pub script_path: PathBuf,
}

impl Plugin for LuaScriptingPlugin {
    fn build(&self, app: &mut App) {
        let lua = Lua::new();
        install_globals(&lua).expect("install lua globals");
        app.insert_non_send_resource(LuaRuntime { lua });
        app.insert_resource(ScriptFile {
            path: self.script_path.clone(),
            last_modified: None,
            loaded: false,
        });
        app.add_systems(Update, reload_if_changed);
        app.add_systems(Update, drive_on_update);
    }
}

fn install_globals(lua: &Lua) -> LuaResult<()> {
    let globals = lua.globals();

    // Component constructors: Position(x, y) and Velocity(x, y) create new userdata.
    globals.set(
        "Position",
        lua.create_function(|_, (x, y): (f64, f64)| Ok(Position { x, y }))?,
    )?;
    globals.set(
        "Velocity",
        lua.create_function(|_, (x, y): (f64, f64)| Ok(Velocity { x, y }))?,
    )?;

    // The world handle.
    let world = lua.create_userdata(WorldProxy)?;
    globals.set("world", world)?;

    Ok(())
}

fn reload_if_changed(world: &mut World) {
    let (path, prev_mtime) = {
        let sf = world.resource::<ScriptFile>();
        (sf.path.clone(), sf.last_modified)
    };
    let Ok(meta) = fs::metadata(&path) else {
        return;
    };
    let mtime = meta.modified().ok();
    if mtime == prev_mtime {
        return;
    }
    let Ok(src) = fs::read_to_string(&path) else {
        return;
    };

    {
        let mut sf = world.resource_mut::<ScriptFile>();
        sf.last_modified = mtime;
        sf.loaded = true;
    }
    info!("reloading script: {}", path.display());

    with_world(world, || {
        let lua = &world_resource_lua();
        if let Err(e) = lua.load(&src).set_name(path.to_string_lossy().as_bytes()).exec() {
            error!("script load failed: {e}");
            return;
        }
        if let Ok(on_load) = lua.globals().get::<_, Function>("on_load") {
            if let Err(e) = on_load.call::<_, Value>(()) {
                error!("on_load error: {e}");
            }
        }
    });
}

fn drive_on_update(world: &mut World) {
    if !world.resource::<ScriptFile>().loaded {
        return;
    }
    let dt = world.resource::<Time>().delta_secs() as f64;
    with_world(world, || {
        let lua = &world_resource_lua();
        if let Ok(on_update) = lua.globals().get::<_, Function>("on_update") {
            if let Err(e) = on_update.call::<_, Value>(dt) {
                error!("on_update error: {e}");
            }
        }
    });
}

/// Borrow the `Lua` out of `LuaRuntime` via the world pointer set by `with_world`.
/// This is the bridge that lets the Lua callbacks see Bevy's `&mut World`: we
/// reach back through the same pointer to grab the runtime resource.
fn world_resource_lua() -> LuaCloneHack {
    WORLD_CELL.with(|c| {
        let ptr = c.borrow().expect("world_resource_lua called outside with_world");
        // SAFETY: same invariant as `with_world_mut`.
        let world: &mut World = unsafe { &mut *ptr };
        LuaCloneHack(world.non_send_resource::<LuaRuntime>().lua.clone())
    })
}

/// `Lua` is `!Send + !Sync` and we want to deref it inline; this newtype just
/// keeps the borrow checker happy across the closure boundary.
struct LuaCloneHack(Lua);
impl std::ops::Deref for LuaCloneHack {
    type Target = Lua;
    fn deref(&self) -> &Lua {
        &self.0
    }
}

// ============================================================================
// `main`: build a small headless Bevy app that ticks a script
// ============================================================================

fn main() {
    App::new()
        .add_plugins(
            MinimalPlugins
                .set(ScheduleRunnerPlugin::run_loop(Duration::from_millis(100))),
        )
        .add_plugins(bevy::log::LogPlugin::default())
        .add_plugins(LuaScriptingPlugin {
            script_path: PathBuf::from("scripts/game.lua"),
        })
        .add_systems(Update, tick_counter_exit)
        .insert_resource(TickCounter(0))
        .run();
}

#[derive(Resource)]
struct TickCounter(u32);

fn tick_counter_exit(mut counter: ResMut<TickCounter>, mut exit: MessageWriter<AppExit>) {
    counter.0 += 1;
    if counter.0 >= 30 {
        exit.write(AppExit::Success);
    }
}
