-- Shared by the panel and bar widget. UPS controls are never automatically retried.
local M = {}
local counter = 0

local function run(argv, callback)
    local started = noctalia.runAsync(argv, function(result)
        if result.exitCode ~= 0 or result.timedOut or result.stdoutTruncated then
            callback(nil, result.stderr ~= "" and result.stderr or "UPS request failed")
            return
        end
        local value, err = noctalia.json.decode(result.stdout)
        callback(value, err)
    end)
    if not started then callback(nil, "Could not start UPS client") end
end

function M.newRequestId()
    counter += 1
    return string.format("noctalia-%.0f-%d-%d", noctalia.nowMs(), counter, math.random(1, 2147483647))
end

function M.history(limit, callback)
    run({ "sms-gamer", "history", "--limit", tostring(limit) }, callback)
end

function M.events(limit, callback)
    run({ "sms-gamer", "events", "--limit", tostring(limit) }, callback)
end

function M.batteryHistory(limit, beforeId, callback)
    local argv = { "sms-gamer", "battery-history", "--limit", tostring(limit) }
    if beforeId then table.insert(argv, "--before-id"); table.insert(argv, tostring(beforeId)) end
    run(argv, callback)
end

function M.configure(config, revision, callback)
    local json, err = noctalia.json.encode(config)
    if not json then callback(nil, err); return end
    run({ "sms-gamer", "config", "set", "--json", json, "--revision", tostring(revision) }, callback)
end

-- Keep requestId when retrying an uncertain operation. The daemon deduplicates it.
function M.control(action, parameters, requestId, callback)
    local argv = { "sms-gamer", "control", "--request-id", requestId, action }
    if action == "shutdown" then
        table.insert(argv, "--delay-seconds")
        table.insert(argv, tostring(parameters.delay_seconds))
        if parameters.restore_after_minutes ~= nil then
            table.insert(argv, "--restore-after-minutes")
            table.insert(argv, tostring(parameters.restore_after_minutes))
        end
        table.insert(argv, "--confirm-model")
        table.insert(argv, parameters.confirmation)
    elseif action == "timed-test" then
        table.insert(argv, "--minutes")
        table.insert(argv, tostring(parameters.duration_minutes))
        table.insert(argv, "--confirm-model")
        table.insert(argv, parameters.confirmation)
    elseif action == "discharge-test" then
        table.insert(argv, "--confirm-model")
        table.insert(argv, parameters.confirmation)
    elseif action ~= "toggle-beeper" and action ~= "self-test" and action ~= "cancel-test" and action ~= "cancel-shutdown" then
        callback(nil, "Unknown UPS control")
        return
    end
    run(argv, callback)
end

function M.service(action, callback)
    if action ~= "start" and action ~= "restart" and action ~= "enable" and action ~= "disable" then
        callback(nil, "Unknown service action")
        return
    end
    local accepted = noctalia.runAsync({ "systemctl", "--user", action, "sms-gamer.service" }, function(result)
        if result.exitCode ~= 0 or result.timedOut then
            callback(nil, result.stderr ~= "" and result.stderr or "Service operation failed")
        else
            callback(true, nil)
        end
    end)
    if not accepted then callback(nil, "Could not start systemctl") end
end

return M
