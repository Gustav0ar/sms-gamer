local snapshot = noctalia.state.get("snapshot")
local transport = noctalia.state.get("transport")

local function duration(seconds)
    seconds = math.max(0, seconds or 0)
    return string.format("%d:%02d", math.floor(seconds / 60), seconds % 60)
end

local function render()
    local connected = transport and transport.available and snapshot and snapshot.connection == "online"
    local sample = snapshot and snapshot.sample
    barWidget.setGlyph("battery-automotive")
    barWidget.setColor("#ffffff")
    barWidget.setGlyphColor(connected and "#b8d4c1" or "#e1b27b")
    if sample then
        local t = sample.telemetry
        local runtime = sample.runtime
        local protection = snapshot.protection
        if protection and protection.phase == "shutdown_requested" then
            barWidget.setText(protection.dry_run and "PC shutdown test" or "PC shutting down")
        elseif protection and protection.phase == "shutdown_failed" then
            barWidget.setText("PC shutdown failed")
        elseif protection and protection.warning_active then
            barWidget.setText(string.format("%.0f%%  %s  %s", t.battery_charge_percent,
                duration(protection.remaining_seconds), protection.phase == "unverified" and "Save work" or "PC " .. duration(protection.shutdown_in_seconds)))
        elseif runtime and runtime.battery_mode and connected then
            barWidget.setText(string.format("%.0f%%  ~%d min", t.battery_charge_percent, math.ceil(runtime.remaining_seconds / 60)))
            barWidget.setGlyphColor("#e1b27b")
        else
            barWidget.setText(string.format("%.0f%%  %.0f%%%s", t.battery_charge_percent, t.load_percent, connected and "" or " !"))
        end
        local tooltip = string.format("SMS Gamer%s\nBattery %.1f%% · Load %.1f%%\nInput %.1f V · Output %.1f V\n%.1f Hz · %.1f °C", connected and "" or " • last reading", t.battery_charge_percent, t.load_percent, t.input_voltage_v, t.output_voltage_v, t.output_frequency_hz, t.temperature_c)
        if protection and (protection.warning_active or protection.phase == "shutdown_requested" or protection.phase == "shutdown_failed") then
            barWidget.setColor("#ff5555")
            barWidget.setGlyphColor("#ff5555")
            tooltip ..= "\nSave your work. Estimated battery " .. duration(protection.remaining_seconds)
            if protection.last_error then tooltip ..= "\n" .. protection.last_error end
            if protection.telemetry_stale then tooltip ..= "\nUPS readings lost; countdown uses the last battery estimate." end
        end
        barWidget.setTooltip(tooltip)
    else
        barWidget.setText("UPS --")
        barWidget.setTooltip(transport and transport.message or "Waiting for UPS daemon")
    end
end

noctalia.state.watch("snapshot", function(value) snapshot = value; render() end)
noctalia.state.watch("transport", function(value) transport = value; render() end)

function onClick()
    noctalia.state.set("requested_view", "overview")
    noctalia.togglePanel("smsups/sms-gamer:panel")
end
function onRightClick()
    noctalia.state.set("requested_view", "settings")
    noctalia.togglePanel("smsups/sms-gamer:panel")
end

render()
