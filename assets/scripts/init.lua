-- Demo init: spawn a handful of entities, each tagged with a named behavior.
-- The Rust side ticks LuaBehavior entities every frame by looking up the named
-- function in the `behaviors` table that behaviors.lua defines.

math.randomseed(os.time())

function on_load()
    -- Three wanderers (drift around with small random walks).
    for _ = 1, 3 do
        world:spawn(
            Position(rand_pos(), rand_pos()),
            Velocity((math.random() * 2 - 1) * 60, (math.random() * 2 - 1) * 60),
            LuaBehavior("wander")
        )
    end

    -- Three orbiters (circle the origin at fixed radii).
    for i = 1, 3 do
        local r = 80 + i * 40
        world:spawn(
            Position(r, 0),
            Velocity(0, 0),
            LuaBehavior("orbit")
        )
    end

    -- Two ping-pongers (bounce off invisible walls).
    for _ = 1, 2 do
        world:spawn(
            Position(rand_pos(), rand_pos()),
            Velocity((math.random() * 2 - 1) * 120, (math.random() * 2 - 1) * 120),
            LuaBehavior("bounce")
        )
    end

    print("[lua] init: spawned wanderers, orbiters, and ping-pongers")
end

function rand_pos()
    return (math.random() * 2 - 1) * 200
end
