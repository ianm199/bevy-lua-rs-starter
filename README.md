# bevy-lua-rs-starter

A minimal Bevy + `lua-rs` integration showing what an ECS-direct Lua scripting
backend can look like without going through `bevy_mod_scripting`.

This is a response to [lua-rs#23](https://github.com/ianm199/lua-rs/issues/23),
where the ask was specifically for *"a different API based on ECS with direct
integration with specific crate (without being language agnostic)"*.

**▶ Live demo: <https://ianm199.github.io/bevy-lua-rs-starter/>** — the same app
compiled to WebAssembly, running the Lua-driven ECS simulation on a canvas right
in your browser.

## What it does

Runs a headless Bevy 0.18 app that loads `scripts/game.lua`, calls `on_load()`,
then calls `on_update(dt)` every tick. The script spawns a few entities with
`Position` and `Velocity` components, moves them each frame, and the Bevy app
prints a snapshot every second.

```
$ cargo run
INFO bevy_lua_rs_starter: reloading script: scripts/game.lua
[lua] on_load: spawning a few entities
[lua] spawned Entity(0v0) with |v| = 4.11
[lua] spawned Entity(1v0) with |v| = 4.88
...
[lua] t=1.13s
  Entity(0v0)  Position(14.46, 1.35)
  Entity(1v0)  Position(25.27, -1.70)
  ...
```

Edit `scripts/game.lua` while the app is running and it hot-reloads.

## The API a script sees

```lua
-- Component constructors: real Bevy `Component` types exposed as Lua userdata
-- via #[derive(LuaUserData)] / #[lua_methods].
local p = Position(1.0, 2.0)   -- p.x, p.y, tostring(p) all work
local v = Velocity(0.5, 0.0)   -- v:magnitude() too

-- Spawn an entity with a variadic list of components.
local e = world:spawn(p, v)

-- Iterate entities with a given component.
world:each("Velocity", function(entity, vel)
    local pos = world:get(entity, "Position")
    pos.x = pos.x + vel.x * dt
    pos.y = pos.y + vel.y * dt
    world:set(entity, "Position", pos)
end)
```

`world:get` / `world:set` / `world:despawn` all do what you'd guess.

## How it's wired together on the Rust side

`src/main.rs` is one file, about 300 lines. The interesting parts:

- **Components** are normal Bevy `Component` types that also derive
  `LuaUserData`. The derive generates the `UserData` impl that exposes fields
  (`p.x`, `p.y`) and the `IntoLua` blanket then lets them flow through Lua as
  userdata.
- **`#[lua_methods]` on a `Velocity` impl block** exposes `magnitude()` as a
  Lua-callable `v:magnitude()`. Inherent fns stay callable from Rust as well.
- **`#[lua_impl(Display)]`** wires `__tostring` to the Rust `Display` impl, so
  `tostring(pos)` in a script just calls `format!("{}", pos)`.
- **The `world` global** is a tiny userdata (`WorldProxy`) whose methods
  re-borrow Bevy's `&mut World` through a thread-local pointer that the Bevy
  system sets just before calling into Lua and clears just after. That keeps
  the integration single-threaded and avoids trying to thread `&mut World`
  through Lua values.
- **Hot reload** is mtime polling on the script file; no asset server / no
  notify, just a one-system check each frame.

## Adding a new component

In `src/main.rs`, in the four places that match on the component name string:

```rust
"YourComponent" => {
    let mut q = world.query::<(Entity, &YourComponent)>();
    ...
}
```

A macro could collapse those four arms; explicit code here is exactly what
[#23](https://github.com/ianm199/lua-rs/issues/23) asked for — type-safe
Rust-side bindings, one language, no reflect introspection. A real game would
likely wrap this in a `register_lua_component::<T>("Name")` extension trait,
which is a clean follow-on.

## What this is and isn't

- It **is** a working sketch of the ECS-direct Lua integration shape lua-rs
  was designed to support. About 300 lines, one binary, no extra crates.
- It **isn't** a finished library. There's no `register_lua_component::<T>()`
  builder, no replication-aware scripting story, no event-bus integration, no
  query of more than one component at a time. Each of those is a small step,
  not a hard one.

## Build / run

```sh
cargo run
```

Needs Bevy 0.18 and a recent Rust. `lua-rs-runtime = "0.0.18"` from crates.io
pulls in the new derive macros.

## License

Dual-licensed under MIT or Apache-2.0, your choice. Pick whatever fits the
project you'd put this into.
