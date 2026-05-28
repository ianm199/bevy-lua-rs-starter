-- Three behaviors entities can opt in to via `LuaBehavior("name")`. The Rust
-- tick system looks up `behaviors[name]` per entity each frame.

behaviors = behaviors or {}

local BOUND = 240.0

function behaviors.wander(entity, dt)
    local v = world:get(entity, "Velocity")
    if v == nil then return end
    -- Occasionally jiggle the direction.
    v.x = v.x + (math.random() * 2 - 1) * 40 * dt
    v.y = v.y + (math.random() * 2 - 1) * 40 * dt
    -- Soft cap on speed.
    local speed = v:magnitude()
    if speed > 100 then
        v.x = v.x * 100 / speed
        v.y = v.y * 100 / speed
    end
    world:set(entity, "Velocity", v)

    local p = world:get(entity, "Position")
    if p == nil then return end
    p.x = p.x + v.x * dt
    p.y = p.y + v.y * dt
    -- Wrap around at the edges.
    if p.x > BOUND then p.x = -BOUND elseif p.x < -BOUND then p.x = BOUND end
    if p.y > BOUND then p.y = -BOUND elseif p.y < -BOUND then p.y = BOUND end
    world:set(entity, "Position", p)
end

function behaviors.orbit(entity, dt)
    local p = world:get(entity, "Position")
    if p == nil then return end
    local r = math.sqrt(p.x * p.x + p.y * p.y)
    if r < 0.1 then r = 0.1 end
    local angle = math.atan(p.y, p.x) + dt * 0.8
    p.x = math.cos(angle) * r
    p.y = math.sin(angle) * r
    world:set(entity, "Position", p)
end

function behaviors.bounce(entity, dt)
    local p = world:get(entity, "Position")
    local v = world:get(entity, "Velocity")
    if p == nil or v == nil then return end
    p.x = p.x + v.x * dt
    p.y = p.y + v.y * dt
    if p.x > BOUND or p.x < -BOUND then v.x = -v.x end
    if p.y > BOUND or p.y < -BOUND then v.y = -v.y end
    world:set(entity, "Position", p)
    world:set(entity, "Velocity", v)
end

-- "spiral" uses the scope-delegate API: world:position(entity) returns a
-- userdata that re-acquires &mut Position from the World on every method
-- call. Field mutation is direct, no clone-and-set round trip needed.
-- Compare to behaviors.bounce above, which uses the older get/set style.
function behaviors.spiral(entity, dt)
    local p = world:position(entity)
    local r = math.sqrt(p.x * p.x + p.y * p.y) + 6 * dt
    local angle = math.atan(p.y, p.x) + dt * 1.4
    p.x = math.cos(angle) * r
    p.y = math.sin(angle) * r
    -- Soft cap radius so the spiral wraps back inward.
    if r > 220 then
        p.x = p.x * 30 / r
        p.y = p.y * 30 / r
    end
end
