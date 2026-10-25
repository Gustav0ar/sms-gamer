local rpc = require("./rpc.luau")

local white, muted, line = "#ffffff", "#aaaaaa", "#303030"
local warning, green, blue = "#e1b27b", "#b8d4c1", "#9abfff"
local snapshot = noctalia.state.get("snapshot")
local transport = noctalia.state.get("transport")
local opened, busy = false, false
local view, graphMetric = "overview", "Load"
local history, events = {}, {}
local message, failure, confirmation = nil, nil, nil
local draft, draftRevision, draftGeneration = nil, nil, 0
local historyRows, requestGeneration = 30, 0
local historyMode = "readings"
local batterySessions, batteryLoaded, batteryHasMore = {}, false, false
local batteryLoading, batteryError, batteryRevision = false, nil, nil
local batteryRequest, selectedBatteryId = 0, nil
local testMinutes, shutdownSeconds, restoreMinutes = "1", "30", "60"
local restoreEnabled, modelConfirmation = true, ""
local hoveredIndex = nil
local showModelAssumptions = false
local render

local metricDefinitions = {
    { "battery_charge_percent", "Battery", "%", 0 },
    { "load_percent", "Load", "%", 1 },
    { "temperature_c", "Temperature", "°C", 1 },
    { "input_voltage_v", "Input", "V", 1 },
    { "output_voltage_v", "Output", "V", 1 },
    { "output_frequency_hz", "Frequency", "Hz", 1 },
}
local settings = {
    { "poll_interval_ms", "Poll interval", "sec", 1000, 60000, 1000 },
    { "request_timeout_ms", "Reply timeout", "sec", 100, 10000, 1000 },
    { "reconnect_interval_ms", "Reconnect delay", "sec", 1000, 60000, 1000 },
    { "history_samples", "History limit", "samples", 1, 10000, 1 },
    { "low_charge_percent", "Low charge", "%", 0, 100, 1 },
    { "high_load_percent", "High load", "%", 0, 200, 1 },
    { "high_temperature_c", "High temperature", "°C", 0, 150, 1 },
}
local runtimeSettings = {
    { "rated_output_w", "Rated output", "W", 1, 100000 },
    { "battery_capacity_ah", "Battery bank capacity", "Ah", 0.1, 2000 },
    { "battery_health_percent", "Battery health assumption", "%", 0, 100 },
    { "inverter_efficiency_percent", "Inverter efficiency", "%", 1, 100 },
    { "idle_power_w", "UPS idle draw", "W", 1, 1000 },
    { "peukert_exponent", "Peukert exponent", "", 1, 2 },
    { "capacity_rating_hours", "Capacity rating period", "hours", 1, 100 },
}

local function label(text, size, color)
    return ui.label({ text = tostring(text), fontSize = size or 12, color = color or white })
end

local function separator()
    return ui.separator({ color = line, thickness = 1 })
end

local function row(name, value)
    return ui.row({ gap = 12, align = "center", justify = "space_between", paddingV = 6 }, {
        ui.label({ text = name, color = muted, fontSize = 12, flexGrow = 1 }),
        type(value) == "table" and value or label(value),
    })
end

local function online()
    return transport and transport.available and snapshot and snapshot.connection == "online"
end

local function sample()
    return snapshot and snapshot.sample
end

local function number(value, decimals)
    if type(value) ~= "number" then return "N/A" end
    return string.format("%." .. tostring(decimals or 1) .. "f", value)
end

local function duration(seconds)
    return string.format("%d:%02d", math.floor(seconds / 60), seconds % 60)
end

local function parseDuration(text)
    local minutes, seconds = text:match("^%s*(%d+):(%d%d)%s*$")
    if not minutes or tonumber(seconds) > 59 then return nil end
    return tonumber(minutes) * 60 + tonumber(seconds)
end

local function timestamp(ms)
    if not ms then return "N/A" end
    return noctalia.formatTime(noctalia.timeFormat(), math.floor(ms / 1000))
end

local function age()
    local s = sample()
    if not s or not s.time_unix_ms then return "No readings yet" end
    local seconds = math.max(0, math.floor((noctalia.nowMs() - s.time_unix_ms) / 1000))
    if online() and seconds < 5 then return "Updated just now" end
    if seconds < 60 then return "Last reading " .. seconds .. "s ago" end
    if seconds < 3600 then return "Last reading " .. math.floor(seconds / 60) .. " min ago" end
    return "Last reading " .. timestamp(s.time_unix_ms)
end

local function refreshHistory()
    local generation = requestGeneration
    local limit = snapshot and snapshot.config.history_samples or 300
    rpc.history(limit, function(result, err)
        if generation ~= requestGeneration then return end
        if err then
            failure = err
        else
            local merged = {}
            for _, item in ipairs(result.samples or {}) do merged[item.sequence] = item end
            for _, item in ipairs(history) do merged[item.sequence] = item end
            history = {}
            for _, item in pairs(merged) do table.insert(history, item) end
            table.sort(history, function(a, b) return a.sequence < b.sequence end)
            while #history > limit do table.remove(history, 1) end
        end
        if opened then render() end
    end)
end

local function refreshEvents()
    local generation = requestGeneration
    rpc.events(100, function(result, err)
        if generation ~= requestGeneration then return end
        if err then failure = err else events = result.events or {} end
        if opened then render() end
    end)
end

local function refreshBatteryHistory(append)
    if batteryLoading or not snapshot or not snapshot.battery_history or not transport or not transport.available then return end
    batteryLoading, batteryError = true, nil
    batteryRequest += 1
    local request, generation = batteryRequest, requestGeneration
    local beforeId = append and batterySessions[#batterySessions] and batterySessions[#batterySessions].id or nil
    rpc.batteryHistory(50, beforeId, function(result, err)
        if request ~= batteryRequest or generation ~= requestGeneration then return end
        batteryLoading = false
        if err then
            batteryError = err
        else
            if not append then batterySessions = {} end
            for _, session in ipairs(result.sessions or {}) do table.insert(batterySessions, session) end
            batteryHasMore, batteryLoaded = result.has_more, true
            batteryRevision = result.status.revision
            batteryError = result.status.last_error
        end
        if opened then render() end
    end)
end

local function setHistoryMode(mode)
    historyMode = mode
    if mode == "battery" then refreshBatteryHistory(false) else refreshHistory() end
    render()
end

local function loadDraft()
    draft = nil
    if not snapshot then return end
    draft = { device = snapshot.config.device or "", auto_device = snapshot.config.device == nil }
    for _, setting in ipairs(settings) do
        draft[setting[1]] = tostring(snapshot.config[setting[1]] / setting[6])
    end
    draft.runtime = {}
    for _, setting in ipairs(runtimeSettings) do draft.runtime[setting[1]] = tostring(snapshot.config.runtime[setting[1]]) end
    local protection = snapshot.config.protection or { enabled = false, warning_seconds = 90, shutdown_seconds = 50, battery_mode_verified = false }
    draft.protection = {
        enabled = protection.enabled, warning = duration(protection.warning_seconds),
        shutdown = duration(protection.shutdown_seconds), battery_mode_verified = protection.battery_mode_verified,
    }
    draftRevision = snapshot.config_revision
    draftGeneration += 1
end

local function setView(nextView)
    if nextView == "battery_history" then nextView, historyMode = "history", "battery" end
    view, message, failure, confirmation = nextView, nil, nil, nil
    modelConfirmation = ""
    if nextView == "settings" then loadDraft() end
    if nextView == "events" then refreshEvents() end
    if nextView == "history" then
        if historyMode == "battery" then refreshBatteryHistory(false) else refreshHistory() end
    end
    render()
end

local function sendControl(action, parameters)
    if busy or not online() then return end
    busy, message, failure = true, nil, nil
    local id = rpc.newRequestId()
    rpc.control(action, parameters or {}, id, function(result, err)
        busy = false
        if err then
            failure = err .. " Check the UPS before trying again."
        else
            message = "Command written to UPS. Device acknowledgement is unavailable."
            noctalia.state.set("last_receipt", result)
        end
        if opened then render() end
        if view == "events" then refreshEvents() end
    end)
    render()
end

local function review(action, parameters, title, description, requireModel)
    if busy or not online() then return end
    confirmation = {
        action = action, parameters = parameters, title = title, description = description,
        requireModel = requireModel, model = sample().identity.model, device = snapshot.device,
    }
    modelConfirmation = ""
    message, failure = nil, nil
    render()
end

local function applySettings()
    if busy or not draft or not snapshot then return end
    local config = {}
    for _, setting in ipairs(settings) do
        local value = tonumber(draft[setting[1]])
        value = value and value * setting[6]
        if not value or value ~= value or value < setting[4] or value > setting[5]
            or (setting[1]:match("_ms$") or setting[1] == "history_samples") and value % 1 ~= 0 then
            failure = setting[2] .. " must be between " .. setting[4] / setting[6] .. " and " .. setting[5] / setting[6] .. " " .. setting[3] .. "."
            render()
            return
        end
        config[setting[1]] = value
    end
    config.runtime = {}
    for _, setting in ipairs(runtimeSettings) do
        local value = tonumber(draft.runtime[setting[1]])
        if not value or value ~= value or value < setting[4] or value > setting[5] then failure = setting[2] .. " is outside the supported range."; render(); return end
        config.runtime[setting[1]] = value
    end
    local warn, shutdown = parseDuration(draft.protection.warning), parseDuration(draft.protection.shutdown)
    if not warn or not shutdown or warn <= shutdown or warn > 7200 or shutdown < 10 or shutdown > 3600 then
        failure = "Use minutes:seconds. Warning must precede shutdown. Reserve 10–3600 seconds for shutdown, with warning at most 120 minutes."
        render()
        return
    end
    config.protection = { enabled = draft.protection.enabled, warning_seconds = warn, shutdown_seconds = shutdown,
        battery_mode_verified = draft.protection.battery_mode_verified }
    if not draft.auto_device then
        if string.sub(draft.device, 1, 1) ~= "/" then failure = "Choose an absolute serial device path."; render(); return end
        config.device = draft.device
    end
    busy, failure, message = true, nil, nil
    rpc.configure(config, draftRevision, function(result, err)
        busy = false
        if err then
            failure = err
        else
            draftRevision = result.revision
            message = "Monitoring settings saved."
        end
        if opened then render() end
    end)
    render()
end

local function service(action)
    if busy then return end
    busy, failure, message = true, nil, nil
    rpc.service(action, function(ok, err)
        busy = false
        if not ok then failure = err else message = "Service " .. action .. " request completed." end
        if opened then render() end
    end)
    render()
end

local function section(title)
    return ui.label({ text = title, fontSize = 14, fontWeight = "semibold", color = white })
end

local function metric(definition)
    local s = sample()
    local field, title, unit, decimals = table.unpack(definition)
    local t = s and s.telemetry
    local stats = snapshot and snapshot.statistics
    local subtext = ""
    if field == "battery_charge_percent" and s then
        subtext = "Nominal " .. s.ratings.battery_nominal_v .. " V"
    elseif field == "output_frequency_hz" and s then
        subtext = "Nominal " .. s.ratings.output_frequency_nominal_hz .. " Hz"
    elseif stats then
        local range = stats[field]
        subtext = number(range.minimum) .. "–" .. number(range.maximum) .. " " .. unit
    end
    return ui.column({ key = field, flexGrow = 1, gap = 5 }, {
        label(title, 12, muted),
        ui.row({ gap = 4, align = "end" }, {
            ui.label({ text = number(t and t[field], decimals), fontSize = 28, color = white, fontWeight = "medium" }),
            label(unit, 12, muted),
        }),
        label(subtext, 10, muted),
    })
end

local function graph()
    local field, unit = "load_percent", "%"
    if graphMetric == "Voltage" then field, unit = "input_voltage_v", "V"
    elseif graphMetric == "Temperature" then field, unit = "temperature_c", "°C"
    elseif graphMetric == "Battery" then field, unit = "battery_charge_percent", "%"
    elseif graphMetric == "Frequency" then field, unit = "output_frequency_hz", "Hz" end
    local points = {}
    if #history > 0 then
        local stride = math.max(1, math.ceil(#history / 300))
        for index = 1, #history, stride do table.insert(points, history[index]) end
        if points[#points].sequence ~= history[#history].sequence then table.insert(points, history[#history]) end
    end
    local low, high = math.huge, -math.huge
    for _, point in ipairs(points) do
        local value = point.telemetry[field]
        low, high = math.min(low, value), math.max(high, value)
        if graphMetric == "Voltage" then
            low, high = math.min(low, point.telemetry.output_voltage_v), math.max(high, point.telemetry.output_voltage_v)
        end
    end
    local values, values2 = {}, {}
    if #points > 0 then
        if high == low then low, high = low - 0.5, high + 0.5 end
        for _, point in ipairs(points) do
            table.insert(values, (point.telemetry[field] - low) / (high - low))
            if graphMetric == "Voltage" then table.insert(values2, (point.telemetry.output_voltage_v - low) / (high - low)) end
        end
    end
    local caption = #points > 0 and number(low) .. "–" .. number(high) .. " " .. unit or "Waiting for readings"
    if hoveredIndex and points[hoveredIndex] then
        local point = points[hoveredIndex]
        caption = timestamp(point.time_unix_ms) .. "   " .. number(point.telemetry[field]) .. " " .. unit
    end
    local options = {}
    local choices = view == "history" and { "Load", "Voltage", "Temperature", "Battery", "Frequency" } or { "Load", "Voltage", "Temperature" }
    for _, choice in ipairs(choices) do
        table.insert(options, ui.button({ key = "plot-" .. choice, text = choice, fontSize = 10, controlSize = "sm", variant = "ghost", selected = graphMetric == choice,
            onClick = function() graphMetric = choice; hoveredIndex = nil; render() end }))
    end
    local children = {
        ui.row({ align = "center", justify = "space_between", gap = 8 }, { section("Recent readings"), ui.row({ gap = 0 }, options) }),
        label(caption, 10, muted),
    }
    if #points > 0 then
        table.insert(children, ui.graph({ key = "history-graph", values = values, values2 = values2, color = blue, color2 = green, lineWidth = 1.5, fillOpacity = 0, height = 105,
            onPointerMove = function(x)
                hoveredIndex = math.max(1, math.min(#points, math.floor((tonumber(x) or 0) * (#points - 1)) + 1))
                render()
            end,
            onPointerLeave = "onGraphLeave",
        }))
        table.insert(children, ui.row({ justify = "space_between" }, { label(timestamp(points[1].time_unix_ms), 10, muted), label(timestamp(points[#points].time_unix_ms), 10, muted) }))
    else
        table.insert(children, ui.box({ height = 105, fill = "#000000" }))
    end
    return ui.column({ gap = 4 }, children)
end

local function runtimeView()
    local s = sample()
    local estimate = s and s.runtime
    if not estimate then return label("Runtime estimate unavailable.", 12, muted) end
    local seconds = estimate.remaining_seconds
    if online() and estimate.battery_mode then seconds = math.max(0, seconds - math.max(0, (noctalia.nowMs() - s.time_unix_ms) / 1000)) end
    local duration = seconds < 60 and "Less than 1 min" or "About " .. math.ceil(seconds / 60) .. " min"
    return ui.column({ gap = 5 }, {
        label(estimate.battery_mode and "Estimated time remaining" or "Estimated battery runtime at this load", 12, muted),
        ui.label({ text = duration, fontSize = 27, fontWeight = "medium", color = estimate.battery_mode and warning or white }),
        label("Approx. " .. number(estimate.estimated_load_w, 0) .. " W · configured battery model", 11, muted),
        label(estimate.battery_mode and (estimate.mode_verified and "Calculated from the configured battery model." or "Battery mode follows an unverified vendor flag.") or "Prediction for battery operation, not a measured runtime.", 10, muted),
    })
end

local function overview()
    local s = sample()
    local nodes = {
        ui.row({ gap = 16 }, { metric(metricDefinitions[1]), metric(metricDefinitions[2]), metric(metricDefinitions[3]) }),
        ui.row({ gap = 16 }, { metric(metricDefinitions[4]), metric(metricDefinitions[5]), metric(metricDefinitions[6]) }),
        graph(),
        row("Status byte", s and s.telemetry.status_byte .. " · mapping unverified" or "N/A"),
    }
    if s and s.runtime and s.runtime.battery_mode then table.insert(nodes, 1, runtimeView()) end
    return ui.column({ gap = 22 }, nodes)
end

local function recordedDuration(ms)
    if ms == 0 then return "0 s" end
    if ms < 1000 then return "<1 s" end
    local seconds = math.floor(ms / 1000)
    if seconds < 60 then return seconds .. " s" end
    local minutes = math.floor(seconds / 60)
    if minutes < 60 then return minutes .. " min " .. seconds % 60 .. " s" end
    local hours = math.floor(minutes / 60)
    if hours < 24 then return hours .. " h " .. minutes % 60 .. " min" end
    return math.floor(hours / 24) .. " d " .. hours % 24 .. " h"
end

local function coverage(totals)
    local elapsed = totals.observed_ms + totals.missing_ms
    if elapsed == 0 then return "N/A" end
    local percent = 100 * totals.observed_ms / elapsed
    -- Keep a rounded value from implying complete coverage when time is missing.
    if totals.missing_ms > 0 then percent = math.min(percent, 99.9) end
    return number(percent) .. "%"
end

local function energyReading(totals, size)
    return ui.row({ gap = 6, align = "center" }, {
        ui.label({ text = number(totals.estimated_kwh, 3), fontSize = size, fontWeight = "medium", color = white }),
        label("kWh", 12, muted),
    })
end

local function energyView()
    local energy = snapshot.energy
    if not energy then
        return ui.column({ gap = 10 }, {
            section("Energy data unavailable"),
            label("Update and restart the UPS daemon to enable energy tracking.", 12, muted),
        })
    end
    local counters = energy.counters
    if not counters then
        return ui.column({ gap = 10 }, {
            section("Energy tracking unavailable"),
            label(energy.last_error or "Waiting for energy counters.", 12, warning),
            label("Saved counters are preserved. Check energy storage and restart the daemon.", 11, muted),
        })
    end
    local s = sample()
    local freshnessMs = snapshot.config.poll_interval_ms + snapshot.config.request_timeout_ms + 100
    local stale = not s or noctalia.nowMs() - s.time_unix_ms > freshnessMs
    local paused = counters.gap_active or not online() or stale
    local today = counters.today
    local quality = paused and "Recording paused · waiting for fresh readings"
        or today.observed_ms == 0 and "Waiting for two fresh readings"
        or coverage(today) .. " recorded today · " .. recordedDuration(today.missing_ms) .. " missing"
    local watts = s and s.runtime and s.runtime.estimated_load_w
    local rows = {}
    local function coverageRow(values, heading)
        local columns = {}
        for index, value in ipairs(values) do
            table.insert(columns, ui.column({ width = index == 1 and 96 or index == 4 and 76 or 130,
                align = index == 1 and "start" or "end" }, { label(value, heading and 10 or 12, heading and muted or white) }))
        end
        return ui.row({ gap = 10, paddingV = heading and 2 or 3 }, columns)
    end
    table.insert(rows, section("Recording coverage"))
    table.insert(rows, coverageRow({ "Period", "Observed", "Missing", "Coverage" }, true))
    table.insert(rows, separator())
    for _, period in ipairs({ { "Today", today }, { "This month", counters.month_to_date }, { "Total", counters.total } }) do
        local totals = period[2]
        table.insert(rows, coverageRow({ period[1], recordedDuration(totals.observed_ms), recordedDuration(totals.missing_ms), coverage(totals) }, false))
        table.insert(rows, separator())
    end
    -- The daemon supplies RFC 3339 UTC timestamps; keep the timezone explicit.
    local since = counters.tracked_since:sub(1, 16):gsub("T", " ") .. " UTC"
    local nodes = {
        ui.row({ gap = 20, justify = "space_between", align = "start" }, {
            ui.column({ gap = 0 }, { label("Estimated output energy today", 12), energyReading(today, 40) }),
            ui.column({ gap = 2, align = "end" }, {
                label(number(watts, 0) .. " W", 27),
                label(paused and "Last estimated power" or "Estimated power now", 11, muted),
            }),
        }),
        label(quality, 11, paused and warning or muted),
        ui.row({ gap = 24 }, {
            ui.column({ gap = 0, flexGrow = 1 }, { energyReading(counters.month_to_date, 27), label("This month · " .. counters.month, 11, muted) }),
            ui.column({ gap = 0, flexGrow = 1 }, { energyReading(counters.total, 27), label("Total", 11, muted) }),
        }),
        separator(),
        ui.column({ gap = 2 }, rows),
        ui.column({ gap = 2 }, {
            label("Tracking since " .. since, 11, muted),
            label("Local day " .. counters.local_date .. ". Missing time is excluded from kWh.", 11, muted),
            label("Estimates use UPS load %. UPS losses and charging are excluded.", 11, muted),
        }),
    }
    if energy.last_error then table.insert(nodes, label(energy.last_error, 11, warning)) end
    return ui.column({ gap = 10 }, nodes)
end

local function detailView()
    local s = sample()
    if not s then return label("Device details appear after the first reading.", 12, muted) end
    local nodes = { section("Device and ratings") }
    for _, pair in ipairs({
        { "Model", s.identity.model }, { "Firmware", s.identity.firmware },
        { "Topology", s.topology:gsub("_", " ") },
        { "Nominal battery", s.ratings.battery_nominal_v .. " V" },
        { "Nominal frequency", s.ratings.output_frequency_nominal_hz .. " Hz" },
        { "Last input voltage", number(s.telemetry.last_input_voltage_v) .. " V · meaning unverified" },
        { "Voltage range code", s.ratings.voltage_range_code }, { "Current range code", s.ratings.current_range_code },
    }) do table.insert(nodes, row(pair[1], pair[2])) end
    table.insert(nodes, separator())
    table.insert(nodes, section("Status flags"))
    table.insert(nodes, label(snapshot.config.protection and snapshot.config.protection.battery_mode_verified
        and "Battery flag verified. Other vendor flag meanings are unverified."
        or "Vendor names. Behavior on this Gamer firmware is unverified.", 11, warning))
    local flags = {
        { "Beeper enabled", "beeper_enabled" }, { "Shutdown active", "shutdown_active" },
        { "Self-test active", "self_test_active" }, { "UPS status", "ups_ok" },
        { "Boost", "boost" }, { "Bypass", "bypass" }, { "Low battery", "low_battery" }, { "On battery", "on_battery" },
    }
    for index = 1, #flags, 2 do
        table.insert(nodes, ui.row({ gap = 24 }, {
            ui.column({ flexGrow = 1 }, { row(flags[index][1], s.vendor_flags[flags[index][2]] and "1" or "0") }),
            ui.column({ flexGrow = 1 }, { row(flags[index + 1][1], s.vendor_flags[flags[index + 1][2]] and "1" or "0") }),
        }))
    end
    table.insert(nodes, row("Raw bits", s.telemetry.status_bits_msb_first))
    table.insert(nodes, row("Frame header", s.telemetry.frame_type))
    table.insert(nodes, separator())
    table.insert(nodes, section("Session extrema"))
    local stats = snapshot.statistics
    if stats then
        for _, def in ipairs(metricDefinitions) do
            local range = stats[def[1]]
            table.insert(nodes, row(def[2], number(range.minimum) .. "–" .. number(range.maximum) .. " " .. def[3]))
        end
        table.insert(nodes, row("Readings", tostring(stats.samples)))
    end
    table.insert(nodes, separator())
    table.insert(nodes, section("Runtime estimate"))
    table.insert(nodes, runtimeView())
    if s.runtime then
        table.insert(nodes, row("Nominal battery energy", number(s.runtime.nominal_capacity_wh, 0) .. " Wh"))
        table.insert(nodes, row("Estimated battery draw", number(s.runtime.estimated_battery_power_w, 0) .. " W"))
        table.insert(nodes, row("Estimated usable charge", number(s.runtime.estimated_remaining_charge_percent) .. "%"))
    end
    table.insert(nodes, separator())
    table.insert(nodes, section("Connection"))
    table.insert(nodes, row("Port", snapshot.device or "Auto-detect"))
    table.insert(nodes, row("Serial framing", "2400 baud · 8N1"))
    table.insert(nodes, label("Battery voltage, watts, current, runtime, battery health, and fan speed are unavailable over the known USB queries.", 11, muted))
    return ui.column({ gap = 6 }, nodes)
end

local function settingsView()
    if not draft then return label("Connect to the daemon to configure monitoring.", 12, muted) end
    local nodes = { section("Monitoring") }
    if snapshot and draftRevision ~= snapshot.config_revision then
        table.insert(nodes, label("Settings changed in another app. Reload before saving.", 11, warning))
    end
    for index, setting in ipairs(settings) do
        local key, title, unit = setting[1], setting[2], setting[3]
        if index == 5 then
            table.insert(nodes, separator())
            table.insert(nodes, section("Alert thresholds"))
            table.insert(nodes, label("Set a threshold to 0 to disable that warning.", 10, muted))
        end
        local editor = ui.row({ gap = 7, align = "center" }, {
            ui.input({ key = "setting-" .. key .. "-" .. draftGeneration, value = draft[key], width = 95, controlSize = "sm", enabled = not busy,
                onChange = function(value) draft[key] = value end }),
            ui.label({ text = unit, width = 45, fontSize = 11, color = muted }),
        })
        table.insert(nodes, row(title, editor))
    end
    table.insert(nodes, separator())
    table.insert(nodes, section("PC protection"))
    table.insert(nodes, row("Automatic PC shutdown", ui.toggle({ checked = draft.protection.enabled, enabled = not busy,
        onChange = function(value) draft.protection.enabled = value == "true"; render() end })))
    for _, setting in ipairs({ { "warning", "Warn when battery time reaches" }, { "shutdown", "Shut down when battery time reaches" } }) do
        local key = setting[1]
        table.insert(nodes, row(setting[2], ui.row({ gap = 7, align = "center" }, {
            ui.input({ key = "protection-" .. key .. "-" .. draftGeneration, value = draft.protection[key], width = 95,
                controlSize = "sm", enabled = not busy, onChange = function(value) draft.protection[key] = value end }),
            ui.label({ text = "min:sec", width = 45, fontSize = 11, color = muted }),
        })))
    end
    table.insert(nodes, label(draft.protection.battery_mode_verified and "Battery detection verified. Linux performs an orderly PC shutdown."
        or "Battery detection is unverified. Automatic shutdown stays blocked.", 11, draft.protection.battery_mode_verified and muted or warning))
    if snapshot.protection and snapshot.protection.last_error and draft.protection.battery_mode_verified then
        table.insert(nodes, label(snapshot.protection.last_error, 11, warning))
    end
    table.insert(nodes, separator())
    table.insert(nodes, section("Runtime estimation"))
    table.insert(nodes, label("Stock Gamer 2000: 1400 W, two 12 V / 9 Ah batteries in series. Adjust capacity for external or aged batteries.", 11, muted))
    for index, setting in ipairs(runtimeSettings) do
        if index <= 3 or showModelAssumptions then
            local key = setting[1]
            table.insert(nodes, row(setting[2], ui.row({ gap = 7, align = "center" }, {
                ui.input({ key = "runtime-" .. key .. "-" .. draftGeneration, value = draft.runtime[key], width = 95, controlSize = "sm", enabled = not busy, onChange = function(value) draft.runtime[key] = value end }),
                ui.label({ text = setting[3], width = 45, fontSize = 11, color = muted }),
            })))
        end
    end
    table.insert(nodes, ui.button({ text = showModelAssumptions and "Hide model assumptions" or "Model assumptions…", variant = "ghost", onClick = function() showModelAssumptions = not showModelAssumptions; render() end }))
    table.insert(nodes, separator())
    table.insert(nodes, section("USB device"))
    table.insert(nodes, row("Auto-detect", ui.toggle({ checked = draft.auto_device, enabled = not busy, onChange = function(value) draft.auto_device = value == "true"; render() end })))
    if not draft.auto_device then
        table.insert(nodes, ui.input({ key = "device-" .. draftGeneration, value = draft.device, placeholder = "/dev/serial/by-id/…", enabled = not busy,
            onChange = function(value) draft.device = value end }))
    end
    table.insert(nodes, ui.row({ gap = 8, justify = "end" }, {
        ui.button({ text = "Reload settings", variant = "ghost", enabled = not busy, onClick = function() loadDraft(); failure = nil; render() end }),
        ui.button({ text = busy and "Saving…" or "Apply settings", variant = "primary", enabled = not busy and transport and transport.available, onClick = applySettings }),
    }))
    table.insert(nodes, separator())
    table.insert(nodes, section("Background service"))
    table.insert(nodes, ui.row({ gap = 8 }, {
        ui.button({ text = "Start at login", variant = "outline", enabled = not busy, onClick = function() service("enable") end }),
        ui.button({ text = "Disable autostart", variant = "ghost", enabled = not busy, onClick = function() service("disable") end }),
        ui.button({ text = "Notifications", variant = "ghost", onClick = function() noctalia.openSettings() end }),
    }))
    return ui.column({ gap = 7 }, nodes)
end

local function confirmView()
    local action = confirmation
    local connected = online() and snapshot.device == action.device and sample().identity.model == action.model
    local nodes = { section(action.title), label(action.description, 12, muted) }
    if action.requireModel then
        table.insert(nodes, label("Type " .. action.model .. " to confirm.", 12))
        table.insert(nodes, ui.input({ key = "control-confirmation", value = "", placeholder = action.model, enabled = not busy and connected,
            onChange = function(value) modelConfirmation = value; render() end }))
    end
    table.insert(nodes, ui.row({ gap = 8, justify = "end" }, {
        ui.button({ text = "Cancel", variant = "ghost", enabled = not busy, onClick = function() confirmation = nil; modelConfirmation = ""; render() end }),
        ui.button({ text = action.action == "shutdown" and "Schedule shutdown" or "Start test", variant = "destructive", enabled = connected and not busy and (not action.requireModel or modelConfirmation == action.model),
            onClick = function()
                if not connected then return end
                local parameters = action.parameters
                if action.requireModel then parameters.confirmation = action.model end
                confirmation = nil
                sendControl(action.action, parameters)
            end }),
    }))
    return ui.column({ gap = 16 }, nodes)
end

local function controlView()
    if confirmation then return confirmView() end
    local enabled = online() and not busy
    return ui.column({ gap = 12 }, {
        section("Beeper"),
        row("Toggle the audible alarm", ui.button({ text = "Toggle beeper", variant = "outline", enabled = enabled, onClick = function() sendControl("toggle-beeper", {}) end })),
        separator(), section("Battery tests"),
        row("Quick self-test", ui.button({ text = "10-second test", variant = "outline", enabled = enabled,
            onClick = function() review("self-test", {}, "Start a battery self-test?", "The UPS will temporarily use its batteries to run the vendor's ten-second test.", false) end })),
        row("Timed test", ui.row({ gap = 7, align = "center" }, {
            ui.input({ key = "test-minutes", value = testMinutes, width = 70, controlSize = "sm", enabled = enabled, onChange = function(value) testMinutes = value end }),
            label("min", 11, muted),
            ui.button({ text = "Start", variant = "outline", enabled = enabled, onClick = function()
                local minutes = tonumber(testMinutes)
                if not minutes or minutes % 1 ~= 0 or minutes < 1 or minutes > 600 then failure = "Test duration must be 1–600 whole minutes."; render(); return end
                review("timed-test", { duration_minutes = minutes }, "Start a timed battery test?", "The UPS will run on battery for " .. minutes .. " minutes. Check the connected equipment first.", true)
            end }),
        })),
        ui.row({ justify = "space_between", gap = 8 }, {
            ui.button({ text = "Test until low battery…", variant = "ghost", enabled = enabled, onClick = function() review("discharge-test", {}, "Discharge the UPS battery?", "This test continues until the battery is low. Connected equipment may lose power if the battery cannot sustain the load.", true) end }),
            ui.button({ text = "Cancel test", variant = "ghost", enabled = enabled, onClick = function() sendControl("cancel-test", {}) end }),
        }),
        separator(), section("UPS power"),
        label("Cuts power to connected equipment. Shut down your computer first.", 11, warning),
        row("Shut down in", ui.row({ gap = 7, align = "center" }, {
            ui.input({ key = "shutdown-delay", value = shutdownSeconds, width = 95, controlSize = "sm", enabled = enabled, onChange = function(value) shutdownSeconds = value end }), label("sec", 11, muted),
        })),
        row("Restore power after", ui.row({ gap = 7, align = "center" }, {
            ui.toggle({ checked = restoreEnabled, enabled = enabled, onChange = function(value) restoreEnabled = value == "true"; render() end }),
            ui.input({ key = "restore-minutes", value = restoreMinutes, width = 95, controlSize = "sm", enabled = enabled and restoreEnabled, onChange = function(value) restoreMinutes = value end }), label("min", 11, muted),
        })),
        ui.row({ justify = "space_between", gap = 8 }, {
            ui.button({ text = "Cancel shutdown / restore", variant = "ghost", enabled = enabled, onClick = function() sendControl("cancel-shutdown", {}) end }),
            ui.button({ text = "Review shutdown", variant = "destructive", enabled = enabled, onClick = function()
                local seconds, minutes = tonumber(shutdownSeconds), tonumber(restoreMinutes)
                if not seconds or seconds % 1 ~= 0 or seconds < 1 or seconds > 600 then failure = "Shutdown delay must be 1–600 whole seconds."; render(); return end
                if restoreEnabled and (not minutes or minutes % 1 ~= 0 or minutes < 1 or minutes > 10080) then failure = "Restore delay must be 1–10080 whole minutes."; render(); return end
                review("shutdown", { delay_seconds = seconds, restore_after_minutes = restoreEnabled and minutes or nil }, "Shut down UPS outputs?",
                    "Power will be switched off in " .. seconds .. " seconds. " .. (restoreEnabled and "Restore after " .. minutes .. " minutes." or "No automatic restore is scheduled.") .. " Shut down your computer first.", true)
            end }),
        }),
        label("Controls follow the vendor protocol and have not been verified on this Gamer. The UPS provides no acknowledgement.", 11, muted),
    })
end

local function readingsView()
    local nodes = { graph(), separator() }
    local headings = { "Time", "Input V", "Output V", "Load %", "Batt %", "°C", "Hz" }
    local header = {}
    for _, title in ipairs(headings) do table.insert(header, ui.label({ text = title, fontSize = 10, color = muted, flexGrow = 1 })) end
    table.insert(nodes, ui.row({ gap = 6 }, header))
    for index = math.max(1, #history - historyRows + 1), #history do
        local point = history[index]
        local t = point.telemetry
        local values = { timestamp(point.time_unix_ms), number(t.input_voltage_v), number(t.output_voltage_v), number(t.load_percent), number(t.battery_charge_percent, 0), number(t.temperature_c), number(t.output_frequency_hz) }
        local columns = {}
        for _, value in ipairs(values) do table.insert(columns, ui.label({ text = value, fontSize = 10, color = white, flexGrow = 1 })) end
        table.insert(nodes, ui.row({ key = "history-" .. point.sequence, gap = 6, paddingV = 5 }, columns))
    end
    if #history > historyRows then table.insert(nodes, ui.button({ text = "Show more readings", variant = "ghost", onClick = function() historyRows += 30; render() end })) end
    table.insert(nodes, label(#history .. " retained readings. History resets when the daemon stops.", 10, muted))
    return ui.column({ gap = 6 }, nodes)
end

local sessionEndings = {
    mains_restored = "Mains restored", telemetry_lost = "Readings lost", daemon_stopped = "Monitor stopped",
    monitoring_restarted = "Monitor restarted", device_changed = "Device changed", clock_changed = "Clock changed",
}

local function sessionDate(ms, format)
    return noctalia.formatTime(format or "%Y-%m-%d %H:%M:%S", math.floor(ms / 1000))
end

local function sessionDuration(ms)
    local seconds = math.floor(ms / 1000)
    if seconds < 3600 then return duration(seconds) end
    return string.format("%dh %02dm", math.floor(seconds / 3600), math.floor(seconds / 60) % 60)
end

local function batterySessionDetails(session)
    local ending = session["end"]
    local restored = ending and ending.reason == "mains_restored"
    local nodes = {
        ui.row({ gap = 20 }, {
            ui.column({ flexGrow = 1, gap = 3 }, { label("First battery reading", 10, muted), label(sessionDate(session.started_at_ms), 11) }),
            ui.column({ flexGrow = 1, gap = 3 }, { label(restored and "Mains returned" or "Last battery reading", 10, muted), label(sessionDate(restored and ending.time_unix_ms or session.last_seen_at_ms), 11) }),
        }),
        ui.row({ gap = 20 }, {
            ui.column({ flexGrow = 1, gap = 3 }, { label("Observed duration", 10, muted), label(recordedDuration(session.duration_ms), 11) }),
            ui.column({ flexGrow = 1, gap = 3 }, { label("Reported charge", 10, muted), label(number(session.start_charge_percent) .. "% → " .. number(session.last_charge_percent) .. "%", 11) }),
        }),
    }
    if ending and not restored then table.insert(nodes, label("Actual end unknown. Unobserved time is excluded.", 11, warning)) end
    table.insert(nodes, label(session.start_observed and "Transition from mains detected." or "Already on battery when readings began. Start may be earlier.", 10, muted))
    table.insert(nodes, label(session.mode_verified and "Battery mode verified." or "Battery mode was unverified during this session.", 10, session.mode_verified and muted or warning))
    return ui.column({ key = "battery-details-" .. session.id, gap = 9, paddingV = 10 }, nodes)
end

local function batteryHistoryView()
    local status = snapshot.battery_history
    if not status then return label("Update and restart the UPS daemon to enable battery-session history.", 12, muted) end
    local nodes = {}
    if status.last_error then table.insert(nodes, label(status.last_error, 11, warning)) end
    if batteryError and batteryError ~= status.last_error then table.insert(nodes, label(batteryError, 11, warning)) end
    if batteryError then table.insert(nodes, ui.button({ text = "Retry history", variant = "ghost", enabled = not batteryLoading,
        onClick = function() refreshBatteryHistory(false); render() end })) end
    if not status.tracked_since_ms then
        table.insert(nodes, label("Saved history is preserved. Check history storage and restart the daemon.", 11, muted))
        return ui.column({ gap = 10 }, nodes)
    end
    local active = status.active_session
    local s = sample()
    local fresh = online() and s and noctalia.nowMs() - s.time_unix_ms <= snapshot.config.poll_interval_ms + snapshot.config.request_timeout_ms + 100
    if active then
        table.insert(nodes, ui.row({ justify = "space_between", gap = 20, align = "center" }, {
            ui.column({ gap = 5 }, {
                label(fresh and "On battery since " .. sessionDate(active.started_at_ms, "%H:%M:%S") or "Awaiting fresh battery readings", 12, warning),
                label("Charge " .. number(active.start_charge_percent, 0) .. "% → " .. number(active.last_charge_percent, 0) .. "%", 11, muted),
            }),
            ui.column({ gap = 3, align = "end" }, { label(sessionDuration(active.duration_ms), 29), label("Observed duration", 10, muted) }),
        }))
        table.insert(nodes, separator())
    end
    table.insert(nodes, ui.row({ justify = "space_between" }, { section("Battery sessions"), label(status.retained_sessions .. " retained", 11, muted) }))
    local sessions = {}
    local hasActive = false
    for _, session in ipairs(batterySessions) do
        if active and active.id == session.id then session, hasActive = active, true end
        table.insert(sessions, session)
    end
    if active and not hasActive then table.insert(sessions, 1, active) end
    local function cell(content, index)
        return ui.column({ width = ({ 118, 72, 115, 170 })[index], align = index == 1 and "start" or "end" }, { content })
    end
    if #sessions > 0 then
        local headings = {}
        for index, title in ipairs({ "Started", "Duration", "Charge", "Ending" }) do table.insert(headings, cell(label(title, 10, muted), index)) end
        table.insert(nodes, ui.row({ gap = 10 }, headings))
        table.insert(nodes, separator())
        for _, session in ipairs(sessions) do
            local id, ending = session.id, session["end"]
            local selected = selectedBatteryId == id
            local function toggle()
                if selected then selectedBatteryId = nil else selectedBatteryId = id end
                render()
            end
            local date = sessionDate(session.started_at_ms, "%Y-%m-%d") == sessionDate(noctalia.nowMs(), "%Y-%m-%d")
                and "Today" or sessionDate(session.started_at_ms, "%d %b %Y")
            local result = ending and sessionEndings[ending.reason] or active and active.id == id and (fresh and "Running" or "Last observed") or "Updating"
            table.insert(nodes, ui.row({ key = "battery-session-" .. id, gap = 10, paddingV = 5, align = "center" }, {
                cell(ui.column({ gap = 2 }, {
                    ui.button({ text = sessionDate(session.started_at_ms, "%H:%M:%S"), contentAlign = "start", variant = "ghost", fontSize = 12, controlSize = "sm", onClick = toggle }),
                    label(date, 10, muted),
                }), 1),
                cell(label(sessionDuration(session.duration_ms), 12), 2),
                cell(label(number(session.start_charge_percent, 0) .. "% → " .. number(session.last_charge_percent, 0) .. "%", 11), 3),
                cell(ui.row({ gap = 4, align = "center" }, {
                    label(result, 10, ending and ending.reason == "mains_restored" and white or warning),
                    ui.button({ glyph = selected and "chevron-up" or "chevron-down", variant = "ghost", controlSize = "sm", onClick = toggle }),
                }), 4),
            }))
            if selected then table.insert(nodes, batterySessionDetails(session)) end
            table.insert(nodes, separator())
        end
    elseif not batteryLoaded then
        table.insert(nodes, label(batteryError and "History could not be loaded." or "Loading battery sessions…", 12, muted))
    else
        table.insert(nodes, label("No battery sessions yet", 18))
        table.insert(nodes, label("Sessions appear when the UPS reports battery operation. Records survive daemon restarts.", 12, muted))
    end
    if batteryHasMore then table.insert(nodes, ui.button({ text = batteryLoading and "Loading…" or "Load older sessions", variant = "ghost", enabled = not batteryLoading,
        onClick = function() refreshBatteryHistory(true); render() end })) end
    table.insert(nodes, ui.column({ gap = 4 }, {
        label("Interrupted sessions stop at the last confirmed reading.", 11, muted),
        label("Unknown time is excluded. Durations follow polling precision.", 11, muted),
        label("Recording since " .. sessionDate(status.tracked_since_ms, "%d %b %Y") .. ". Keeps the latest 10,000 sessions.", 10, muted),
    }))
    return ui.column({ gap = 10 }, nodes)
end

local function historyView()
    local tabs = {}
    for _, item in ipairs({ { "readings", "Readings" }, { "battery", "Battery sessions" } }) do
        local key, title = item[1], item[2]
        table.insert(tabs, ui.column({ gap = 2 }, {
            ui.button({ text = title, variant = "ghost", fontSize = 12, controlSize = "sm", onClick = function() setHistoryMode(key) end }),
            ui.box({ height = 1, fill = historyMode == key and white or "#000000" }),
        }))
    end
    return ui.column({ gap = 18 }, { ui.row({ gap = 16 }, tabs), historyMode == "battery" and batteryHistoryView() or readingsView() })
end

local function eventsView()
    if #events == 0 then return label("No events recorded in this daemon session.", 12, muted) end
    local nodes = {}
    for index = #events, 1, -1 do
        local event = events[index]
        table.insert(nodes, ui.row({ key = "event-" .. event.id, gap = 14, paddingV = 6, align = "start" }, {
            ui.label({ text = timestamp(event.time_unix_ms), fontSize = 10, color = muted, width = 50 }),
            ui.column({ gap = 3, flexGrow = 1 }, { label(event.message, 12), label(event.kind, 10, muted) }),
        }))
        table.insert(nodes, separator())
    end
    return ui.column({ gap = 6 }, nodes)
end

render = function()
    if not opened then return end
    local connected = online()
    local status = connected and "USB connected" or snapshot and snapshot.connection == "connecting" and "Connecting" or "Disconnected"
    local children = {
        ui.row({ gap = 8, align = "center", justify = "space_between" }, {
            ui.label({ text = "SMS Gamer", fontSize = 18, fontWeight = "semibold", color = white, flexGrow = 1 }),
            ui.glyph({ name = "point-filled", color = connected and green or warning, size = 12 }),
            label(status, 11, connected and green or warning),
            ui.button({ glyph = "x", variant = "ghost", controlSize = "sm", onClick = function() panel.close() end }),
        }),
    }
    local tabs = {}
    for _, tab in ipairs({ { "overview", "Overview" }, { "energy", "Energy" }, { "history", "History" }, { "events", "Events" }, { "settings", "Settings" }, { "controls", "Controls" }, { "details", "Details" } }) do
        local key, title = tab[1], tab[2]
        table.insert(tabs, ui.column({ key = "tab-" .. key, gap = 1, flexGrow = 1 }, {
            ui.button({ text = title, variant = "ghost", fontSize = 11, controlSize = "sm", onClick = function() setView(key) end }),
            ui.box({ height = 1, fill = view == key and white or line }),
        }))
    end
    table.insert(children, ui.row({ gap = 2 }, tabs))
    local body = {}
    if not transport or not transport.available then
        table.insert(body, label(transport and transport.message or "Waiting for UPS daemon", 12, warning))
        table.insert(body, ui.button({ text = "Start daemon service", variant = "outline", enabled = not busy, onClick = function() service("start") end }))
        table.insert(body, label("Requires the supplied sms-gamer systemd user unit.", 11, muted))
    elseif not connected then
        table.insert(body, label(snapshot and snapshot.last_error or "Connecting to the UPS…", 12, warning))
        if sample() then table.insert(body, label("Showing the last successful reading.", 11, muted)) end
    end
    if failure then table.insert(body, label(failure, 11, warning)) end
    if message then table.insert(body, label(message, 11, green)) end
    if snapshot then
        local content = ({ overview = overview, energy = energyView, history = historyView, events = eventsView, settings = settingsView, controls = controlView, details = detailView })[view]
        table.insert(body, content())
    end
    table.insert(children, ui.scroll({ key = "body-" .. view .. (view == "history" and "-" .. historyMode or ""), gap = 14, flexGrow = 1 }, body))
    table.insert(children, separator())
    table.insert(children, ui.row({ justify = "space_between", gap = 8 }, {
        label(sample() and sample().identity.model or "Waiting for identity", 10, muted), label(age(), 10, muted),
    }))
    panel.render(ui.column({ fill = "#000000", gap = 13, padding = 18, radius = 0, flexGrow = 1 }, children))
end

function onGraphLeave() hoveredIndex = nil; render() end

function onIpc(event, payload)
    if event == "view" and ({ overview = true, energy = true, history = true, battery_history = true, events = true, settings = true, controls = true, details = true })[payload] then
        setView(payload)
    end
end

function onOpen(context)
    opened = true
    requestGeneration += 1
    confirmation, modelConfirmation = nil, ""
    message, failure = nil, nil
    local requested = noctalia.state.get("requested_view")
    noctalia.state.set("requested_view", nil)
    view = context == "settings" and "settings" or requested == "settings" and "settings" or "overview"
    if context == "battery_history" then view, historyMode = "history", "battery" end
    if view == "settings" then loadDraft() end
    if transport and transport.available then refreshHistory() end
    if view == "history" and historyMode == "battery" then refreshBatteryHistory(false) end
    render()
end

function onClose()
    opened = false
    confirmation, modelConfirmation = nil, ""
    requestGeneration += 1
    batteryRequest += 1
    batteryLoading = false
end

function update()
    local status = snapshot and snapshot.battery_history
    if opened and view == "history" and historyMode == "battery" and status and status.tracked_since_ms and not batteryError
        and (not batteryLoaded or batteryRevision == nil or status.revision > batteryRevision) then refreshBatteryHistory(false) end
    render()
end
noctalia.setUpdateInterval(1000)

noctalia.state.watch("snapshot", function(value)
    if snapshot and value and value.uptime_ms < snapshot.uptime_ms then
        history, events, draft = {}, {}, nil
        confirmation, modelConfirmation = nil, ""
        requestGeneration += 1
        batteryRequest += 1
        batterySessions, batteryLoaded, batteryHasMore = {}, false, false
        batteryLoading, batteryError, batteryRevision, selectedBatteryId = false, nil, nil, nil
    end
    snapshot = value
    if opened and view == "settings" and not draft and snapshot then loadDraft() end
    local current = sample()
    if current and (#history == 0 or current.sequence ~= history[#history].sequence) then
        table.insert(history, current)
        while #history > snapshot.config.history_samples do table.remove(history, 1) end
        if opened and view == "events" then refreshEvents() end
    end
    render()
end)
noctalia.state.watch("transport", function(value)
    local recovered = value and value.available and (not transport or not transport.available)
    transport = value
    if recovered and opened and view == "history" and historyMode == "battery" then refreshBatteryHistory(false) end
    render()
end)
