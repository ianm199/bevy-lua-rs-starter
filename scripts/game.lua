-- Demo script for bevy-lua-rs-starter.
--
-- Spawns a handful of entities with random velocities at load time, then on
-- every frame moves each Velocity-bearing entity by its velocity. Edit this
-- file while the app is running and it hot-reloads (the next on_load runs
-- against the existing world; nothing is wiped).

math.randomseed(os.time())

function on_load()
    print("[lua] on_load: spawning a few entities")
    for i = 1, 5 do
        local v = Velocity((math.random() * 2 - 1) * 5, (math.random() * 2 - 1) * 5)
        local e = world:spawn(
            Position(i * 10, 0),
            v
        )
        print(string.format("[lua] spawned %s with |v| = %.2f", tostring(e), v:magnitude()))
    end
end

local report_at = 0.0
local accum = 0.0

function on_update(dt)
    -- Apply velocity to position for every Velocity-bearing entity.
    world:each("Velocity", function(entity, vel)
        local pos = world:get(entity, "Position")
        if pos ~= nil then
            pos.x = pos.x + vel.x * dt
            pos.y = pos.y + vel.y * dt
            world:set(entity, "Position", pos)
        end
    end)

    -- Log a snapshot every second or so.
    accum = accum + dt
    if accum >= report_at then
        report_at = accum + 1.0
        print(string.format("[lua] t=%.2fs", accum))
        world:each("Position", function(entity, pos)
            print(string.format("  %s  %s", tostring(entity), tostring(pos)))
        end)
    end
end
