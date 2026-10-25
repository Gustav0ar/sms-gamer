-- One Rust subscription feeds telemetry, energy counters, and battery-session status.
local started = false
local previous = nil
local notifications = noctalia.getConfig("notifications_enabled")
noctalia.setUpdateInterval(5000)
noctalia.state.set("transport", { available = false, message = "Connecting to UPS daemon" })

local function receive(line)
    local event, err = noctalia.json.decode(line)
    if type(event) ~= "table" then
        noctalia.log("Invalid UPS event: " .. tostring(err))
        return
    end
    if event.event == "snapshot" and type(event.snapshot) == "table" then
        local current = event.snapshot
        if notifications and previous then
            if previous.connection == "online" and current.connection == "disconnected" then
                noctalia.notifyError("UPS disconnected", "Showing the last successful reading while the daemon reconnects.")
            elseif previous.connection == "disconnected" and current.connection == "online" then
                noctalia.notify("UPS reconnected", "Live readings have resumed.")
            end
            local before = previous.sample and previous.sample.alerts
            local after = current.sample and current.sample.alerts
            if before and after and current.connection == "online" then
                for _, item in ipairs({ { "low_charge", "UPS charge is low" }, { "high_load", "UPS load is high" }, { "high_temperature", "UPS temperature is high" } }) do
                    if after[item[1]] and not before[item[1]] then noctalia.notifyError(item[2], "Check SMS Gamer for the current reading and configured threshold.") end
                end
            end
        end
        previous = current
        noctalia.state.set("snapshot", event.snapshot)
        noctalia.state.set("transport", { available = true })
    elseif event.event == "unavailable" then
        noctalia.state.set("transport", {
            available = false,
            message = tostring(event.message or "UPS daemon unavailable"),
        })
    end
end

function onConfigChanged()
    notifications = noctalia.getConfig("notifications_enabled")
end

function update()
    if started then return end
    if not noctalia.commandExists("sms-gamer") then
        noctalia.state.set("transport", { available = false, message = "Install sms-gamer and make it available on PATH" })
        return
    end
    -- The command is fixed. Dynamic configuration uses argument arrays in rpc.luau.
    started = noctalia.runStream("sms-gamer watch", receive)
    if not started then
        noctalia.state.set("transport", { available = false, message = "Could not start UPS subscriber" })
    end
end

update()
