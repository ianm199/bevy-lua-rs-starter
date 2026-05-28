//! bevy-lua-rs-starter
//!
//! Bevy 0.18 + lua-rs (scope-API branch), runs natively and on wasm.
//! Demonstrates the ECS-direct scripting shape requested in
//! ianm199/lua-rs#23, refactored on lua-rs#26 (`Lua::scope`).
//!
//! Architecture:
//! - Components (`Position`, `Velocity`, `LuaBehavior`) are real Bevy `Component`
//!   types that derive `LuaUserData`. The derive generates field get/set and
//!   `#[lua_methods]` exposes inherent methods like `Velocity:magnitude()`.
//! - One `Lua` per app, stored as a `NonSend` resource (Lua is `Rc<LuaInner>`).
//! - Multiple Lua scripts loaded into the same VM at startup. Each defines its
//!   own globals/helpers; one shared `behaviors` table holds named per-entity
//!   behaviors keyed by string.
//! - A `world` global is created fresh per system body via `Lua::scope`. The
//!   scope wraps the system's `&mut World` as a userdata that lives only for
//!   that scope. When the scope ends Bevy gets the world back; if a script
//!   stashed the userdata on a global and tries to use it on a later frame,
//!   the call surfaces a clean Lua runtime error instead of touching the
//!   released borrow. This replaces the thread_local pointer indirection the
//!   pre-scope-API version used.
//! - Each frame Rust iterates entities that have a `LuaBehavior` component and
//!   calls `behaviors[name](entity, dt)` for each. That is the per-entity
//!   scripting model (one VM, many scripts, each entity opts in to a behavior).
//! - A scoped Rust closure `log_event` (built fresh per frame) borrows a stack
//!   `&mut Vec<String>` and lets the scripts push frame-local events into it.
//!   This exercises the other half of the scope API — `scope.create_function`.
//! - Bevy renders each entity as a coloured sprite; a sync system mirrors
//!   `Position` into `Transform` so the simulation logic stays in plain Lua
//!   coordinates and the renderer doesn't care.

use std::ptr::NonNull;

use bevy::asset::AssetMetaCheck;
use bevy::color::Color;
use bevy::prelude::*;

use lua_rs_runtime::{
    lua_methods, AnyUserData, FromLua, Function, HostHooks, IntoLua, Lua, LuaError, LuaUserData,
    MetaMethod, Result as LuaResult, Table, UserData, UserDataMethods, Value, Variadic,
};

/// `os.time()` panics on `wasm32-unknown-unknown` without a host-supplied hook
/// (no system clock). Native uses `SystemTime`; wasm uses a monotonic counter
/// rooted at a plausible epoch so `math.randomseed(os.time())` still gets a
/// distinct value per script reload. Real apps should plug in `web_time` or
/// `Performance.now()` via `wasm_bindgen`.
fn host_unix_time() -> i64 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
    #[cfg(target_arch = "wasm32")]
    {
        use std::sync::atomic::{AtomicI64, Ordering};
        static COUNTER: AtomicI64 = AtomicI64::new(1_700_000_000);
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }
}

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
// World userdata (scope-bound)
// ============================================================================

/// `LuaWorld` is the Lua-facing handle to Bevy's `World`. It is *only* created
/// inside `Lua::scope` (see `with_scoped_world` below); the scope guarantees
/// that any access from Lua to this userdata after the scope ends fails with a
/// clean "no longer valid" runtime error rather than touching the released
/// `&mut World`.
///
/// The single unsafe block in this struct's methods derefs `ptr` to get a
/// `&mut World` for the immediate operation. Discipline: never hold the
/// derived `&mut World` across a `callback.call` re-entry into Lua — extract
/// what you need (e.g. query results into an owned `Vec`), drop the borrow,
/// then call back. The pre-scope-API version of this file enforced the same
/// discipline through `with_world_mut`; the shape carries over.
pub struct LuaWorld {
    ptr: NonNull<World>,
}

impl LuaWorld {
    /// SAFETY: caller guarantees `ptr` points at a live `World` for the
    /// duration of the scope into which this `LuaWorld` is handed.
    unsafe fn new(world: &mut World) -> Self {
        Self {
            ptr: NonNull::from(world),
        }
    }

    /// Borrow the world for one Rust-side operation. The returned `&mut
    /// World` must not outlive the calling closure; in particular it must
    /// not be alive when control re-enters Lua, or two `&mut World` borrows
    /// could be reachable at once.
    fn with<R>(&self, f: impl FnOnce(&mut World) -> R) -> R {
        // SAFETY: `ptr` is only constructed inside `with_scoped_world`, which
        // is the only place that owns the originating `&mut World`. Lua
        // method bodies run synchronously inside that exclusive borrow, and
        // we drop the derived `&mut World` before any re-entry into Lua.
        let world: &mut World = unsafe { &mut *self.ptr.as_ptr() };
        f(world)
    }
}

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

impl UserData for LuaWorld {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("spawn", |_lua, this, args: Variadic<Value>| {
            this.with(|world| {
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
            |lua, this, (name, callback): (String, Function)| {
                // Extract pairs while holding `&mut World`, drop the borrow,
                // then run callbacks. This keeps a Lua re-entry from
                // observing two live `&mut World`s through the same pointer.
                let pairs = this.with(|world| query_entries(world, lua, &name))?;
                for (entity, value) in pairs {
                    let _: Value = callback.call((EntityHandle(entity), value))?;
                }
                Ok(())
            },
        );

        m.add_method(
            "get",
            |lua, this, (entity, name): (EntityHandle, String)| {
                this.with(|world| get_component(world, entity.0, &name, lua))
            },
        );

        m.add_method(
            "set",
            |_lua, this, (entity, name, value): (EntityHandle, String, Value)| {
                this.with(|world| set_component(world, entity.0, &name, value))
            },
        );

        m.add_method("despawn", |_lua, this, entity: EntityHandle| {
            this.with(|world| {
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
        let lua = Lua::with_hooks(HostHooks::default().unix_time(host_unix_time))
            .expect("init lua with hooks");
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

    // Override Lua's `print` to go through `bevy::log` rather than the default
    // stdout hook, which is not wired on wasm. The `world` and `log_event`
    // globals are set per-scope in `with_scoped_world` below.
    globals.set(
        "print",
        lua.create_function(|_, msg: String| {
            bevy::log::info!("[lua] {msg}");
            Ok(())
        })?,
    )?;
    Ok(())
}

/// Run a block of Rust code with `world` and `log_event` installed on the Lua
/// globals, both backed by scope-bound borrows of the current system's `&mut
/// World` and `&mut Vec<String>`. After this returns, both globals exist but
/// any call into them surfaces a "no longer valid" Lua error instead of
/// touching the released borrows.
///
/// `log_event` exercises the [`scope.create_function`] half of the API: the
/// captured `&mut Vec<String>` is borrowed from this stack frame, not owned
/// by Lua.
fn with_scoped_world<R>(
    lua: &Lua,
    world: &mut World,
    events: &mut Vec<String>,
    body: impl for<'scope> FnOnce(&Lua) -> LuaResult<R>,
) -> LuaResult<R> {
    // SAFETY: the `LuaWorld` borrows `world` for the lifetime of the
    // surrounding scope body; we hand it to `scope.create_userdata_ref_mut`
    // as a `&'scope mut LuaWorld`, so the scope's cell invalidates on return.
    let mut lua_world = unsafe { LuaWorld::new(world) };
    lua.scope(|scope| {
        let world_ud = scope.create_userdata_ref_mut(lua, &mut lua_world)?;
        let log_event = scope.create_function_mut(lua, |_lua, msg: String| {
            events.push(msg);
            Ok(())
        })?;
        lua.globals().set("world", &world_ud)?;
        lua.globals().set("log_event", &log_event)?;
        body(lua)
    })
}

/// Loads every script into the same VM at startup, in order, then calls
/// `on_load()` with the world handed in via scope.
fn load_scripts(world: &mut World) {
    let lua = world.non_send_resource::<LuaRuntime>().lua.clone();
    let mut events: Vec<String> = Vec::new();
    let result = with_scoped_world(&lua, world, &mut events, |lua| {
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
        Ok(())
    });
    if let Err(e) = result {
        error!("scope error in load_scripts: {e}");
    }
    for event in events {
        bevy::log::info!("[lua-event] {event}");
    }
}

/// Look up each entity's `LuaBehavior` and call `behaviors[name](entity, dt)`.
/// Entities are collected before the scope so we don't hold a query borrow
/// across Lua reentry.
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
    let mut events: Vec<String> = Vec::new();
    let result = with_scoped_world(&lua, world, &mut events, |lua| {
        let Ok(behaviors) = lua.globals().get::<_, Table>("behaviors") else {
            return Ok(());
        };
        for (entity, name) in pairs {
            let Ok(func) = behaviors.get::<_, Function>(name.as_str()) else {
                continue;
            };
            if let Err(e) = func.call::<_, Value>((EntityHandle(entity), dt)) {
                error!("behavior '{name}' on {entity:?}: {e}");
            }
        }
        Ok(())
    });
    if let Err(e) = result {
        error!("scope error in tick_behaviors: {e}");
    }
    for event in events {
        bevy::log::info!("[lua-event] {event}");
    }
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
