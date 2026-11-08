const fs = require('node:fs');
const { spawnSync } = require('node:child_process');
const { tmpdir } = require('node:os');
const { join } = require('node:path');
const snapshot = {
  uptime_ms: 1000, connection: 'online', config_revision: 1,
  config: {history_samples: 100, poll_interval_ms: 2000, request_timeout_ms: 2000},
  sample: {sequence: 1, time_unix_ms: 1700000000000, identity: {model: 'GAM2000BiBiW'}},
  battery_history: {tracked_since_ms: 1699900000000, active_session: null, retained_sessions: 100, revision: 1, last_error: null},
};
const pluginDirectory = 'integrations/noctalia/sms-gamer';
const manifest = fs.readFileSync(join(pluginDirectory, 'plugin.toml'), 'utf8');
const pluginId = manifest.match(/^id = "([^"]+)"/m)?.[1];
if (pluginId !== 'smsups/sms-gamer') throw new Error('Unexpected public plugin ID');
for (const file of [join(pluginDirectory, 'widget.luau'), 'src/host.rs']) {
  if (!fs.readFileSync(file, 'utf8').includes(pluginId + ':panel')) throw new Error(file + ' uses the wrong panel ID');
}
function lua(value) {
  if (value === null || value === undefined) return 'nil';
  if (typeof value !== 'object') return JSON.stringify(value);
  return '{' + Object.entries(value).map(([key, item]) => `[${Array.isArray(value) ? Number(key) + 1 : JSON.stringify(key)}]=${lua(item)}`).join(',') + '}';
}
const rpc = fs.readFileSync('integrations/noctalia/sms-gamer/rpc.luau', 'utf8');
const panel = fs.readFileSync('integrations/noctalia/sms-gamer/panel.luau', 'utf8').replace('local rpc = require("./rpc.luau")', `local rpc = (function()\n${rpc}\nend)()`);
const harness = `
local currentSnapshot = ${lua(snapshot)}
local now = currentSnapshot.sample.time_unix_ms
local values = { snapshot = currentSnapshot, transport = { available = true } }
local watchers, calls, replies = {}, {}, {}
local tree
ui = setmetatable({}, { __index = function(_, kind) return function(props, children) return {kind=kind, props=props, children=children or {}} end end })
panel = { render = function(value) tree = value end, close = function() end }
noctalia = {
    state = { get = function(key) return values[key] end, set = function(key, value) values[key] = value end, watch = function(key, callback) watchers[key] = callback end },
    json = { decode = function(token) return replies[token], nil end },
    nowMs = function() return now end, timeFormat = function() return "%H:%M" end,
    formatTime = function(format) return format == "%Y-%m-%d" and "2023-11-14" or "12:00:00" end,
    setUpdateInterval = function() end,
    runAsync = function(argv, callback) table.insert(calls, {argv=argv, callback=callback}); return true end,
}
local function walk(node, predicate)
    if predicate(node) then return node end
    for _, child in ipairs(node.children or {}) do local found = walk(child, predicate); if found then return found end end
end
local function key(value) return walk(tree, function(node) return node.props.key == value end) end
local function text(value) return walk(tree, function(node) return node.props.text == value end) end
local function button(value) return walk(tree, function(node) return node.kind == "button" and node.props.text == value end) end
local function countRows(node)
    local total = node.props.key and string.match(node.props.key, "^battery%-session%-") and 1 or 0
    for _, child in ipairs(node.children or {}) do total += countRows(child) end
    return total
end
local function pending(command)
    for _, call in ipairs(calls) do if not call.answered and call.argv[2] == command then return call end end
    error("No pending " .. command)
end
local function respond(call, value, err)
    assert(not call.answered, "Request already answered")
    call.answered = true
    local token = tostring(#calls) .. tostring(call)
    replies[token] = value
    call.callback({ exitCode=err and 1 or 0, timedOut=false, stdoutTruncated=false, stderr=err or "", stdout=token })
end
local function session(id, active)
    return { id=id, started_at_ms=now-10000, last_seen_at_ms=now-1000, duration_ms=9000,
        start_charge_percent=100, last_charge_percent=94, start_observed=true, mode_verified=true,
        ["end"]=not active and {reason="mains_restored", time_unix_ms=now} or nil }
end
local function page(first, last, more)
    local records = {}
    for id=first,last,-1 do table.insert(records, session(id)) end
    return { sessions=records, has_more=more, status=values.snapshot.battery_history }
end
${panel}
onOpen("battery_history")
respond(pending("history"), {samples={}})
respond(pending("battery-history"), page(100,51,true))
assert(countRows(tree) == 50, "First page has 50 records")
button("Load older sessions").props.onClick()
local stale = pending("battery-history")
assert(stale.argv[5] == "--before-id" and stale.argv[6] == "51", "Cursor is oldest displayed ID")
onClose()
onOpen("battery_history")
respond(stale, page(50,1,false))
assert(countRows(tree) == 50, "Reply from a closed panel is ignored")
respond(pending("battery-history"), page(100,51,true))
button("Load older sessions").props.onClick()
respond(pending("battery-history"), page(50,1,false))
assert(countRows(tree) == 100, "Second page appends without discarding records")
assert(not button("Load older sessions"), "No extra page after the last record")
local first = key("battery-session-100")
walk(first, function(node) return node.kind == "button" end).props.onClick()
assert(key("battery-details-100"), "Details expand")
walk(key("battery-session-100"), function(node) return node.kind == "button" end).props.onClick()
assert(not key("battery-details-100"), "Details collapse")
values.snapshot.battery_history.active_session = session(101,true)
values.snapshot.battery_history.retained_sessions = 101
values.snapshot.battery_history.revision = 2
watchers.snapshot(values.snapshot)
update()
local refreshed = page(100,52,true)
table.insert(refreshed.sessions,1,session(101,true))
respond(pending("battery-history"),refreshed)
assert(countRows(tree) == 50 and key("battery-session-101"), "New sessions refresh the first page without duplicate active rows")
local requestCount = #calls
values.snapshot.battery_history.active_session.duration_ms = 11000
watchers.snapshot(values.snapshot)
update()
assert(#calls == requestCount, "Active progress uses the snapshot without extra RPCs")
assert(text("0:11"), "Active duration updates")
button("Load older sessions").props.onClick()
respond(pending("battery-history"),nil,"Simulated read failure")
assert(countRows(tree) == 50 and button("Retry history"), "Failed pagination preserves existing records and offers retry")
button("Retry history").props.onClick()
values.snapshot.battery_history.active_session = nil
values.snapshot.battery_history.retained_sessions = 0
values.snapshot.battery_history.revision = 3
respond(pending("battery-history"),{sessions={},has_more=false,status=values.snapshot.battery_history})
assert(text("No battery sessions yet"), "Empty history is explicit")
values.snapshot.battery_history = nil
watchers.snapshot(values.snapshot)
assert(text("Update and restart the UPS daemon to enable battery-session history."), "Older daemons have an explicit unavailable state")
values.snapshot.battery_history = {retained_sessions=0,revision=0,last_error="Saved history is unreadable"}
watchers.snapshot(values.snapshot)
assert(text("Saved history is unreadable"), "Storage errors remain visible")
print("PASS: cursor paging, stale replies, details, active progress, refresh, retry, empty and unavailable states")
`;
const service = fs.readFileSync(join(pluginDirectory, 'service.luau'), 'utf8');
const serviceHarness = `
local function newService(enabled)
    local now, subscriptions = 0, 0
    local messages, values = {}, {}
    local callback, update, onConfigChanged
    local os = { clock = function() return now end }
    local noctalia = {
        getConfig = function() return enabled ~= false end,
        setUpdateInterval = function(ms) assert(ms == 5000) end,
        state = { set = function(key, value) values[key] = value end },
        json = { decode = function(event) return event end },
        commandExists = function() return true end,
        runStream = function(command, receive)
            assert(command == "sms-gamer watch")
            subscriptions += 1
            callback = receive
            return true
        end,
        notify = function(title) table.insert(messages, title) end,
        notifyError = function(title) table.insert(messages, title) end,
        log = function(message) error(message) end,
    }
    ${service}
    return {
        messages = messages, values = values,
        snapshot = function(connection, alerts)
            callback({event="snapshot", snapshot={connection=connection, sample={alerts=alerts or {}}}})
        end,
        unavailable = function() callback({event="unavailable", message="Daemon unavailable"}) end,
        tick = function(seconds) now = seconds; update(); assert(subscriptions == 1, "Ticks must reuse the subscription") end,
        configure = function(value) enabled = value; onConfigChanged() end,
    }
end

local brief = newService()
brief.snapshot("online")
brief.snapshot("disconnected")
assert(#brief.messages == 0, "A disconnect must not notify immediately")
assert(brief.values.snapshot.connection == "disconnected", "The panel must see disconnects immediately")
brief.tick(59.999)
brief.snapshot("online")
brief.tick(120)
assert(#brief.messages == 0, "A brief disconnect and its recovery must stay silent")

local prolonged = newService()
prolonged.snapshot("online")
prolonged.snapshot("disconnected")
prolonged.tick(30)
prolonged.snapshot("disconnected")
prolonged.tick(59.999)
assert(#prolonged.messages == 0, "Repeated snapshots must not notify before one minute")
prolonged.tick(60)
assert(#prolonged.messages == 1 and prolonged.messages[1] == "UPS disconnected", "A prolonged outage must notify even without new snapshots")
prolonged.tick(120)
prolonged.snapshot("disconnected")
assert(#prolonged.messages == 1, "One outage must produce only one disconnect notification")
prolonged.snapshot("online")
prolonged.snapshot("online")
assert(#prolonged.messages == 2 and prolonged.messages[2] == "UPS reconnected", "A notified outage must produce exactly one recovery notification")
prolonged.snapshot("disconnected")
prolonged.tick(179.999)
assert(#prolonged.messages == 2, "A new outage must receive a full new grace period")
prolonged.tick(180)
assert(#prolonged.messages == 3, "A later prolonged outage must notify again")

local interrupted = newService()
interrupted.snapshot("online")
interrupted.snapshot("disconnected")
interrupted.tick(40)
interrupted.snapshot("online")
interrupted.tick(45)
interrupted.snapshot("disconnected")
interrupted.tick(100)
assert(#interrupted.messages == 0, "Separate short outages must not accumulate")
interrupted.snapshot("online")

local startup = newService()
startup.snapshot("disconnected")
startup.tick(60)
assert(#startup.messages == 1, "Starting offline must still report a prolonged outage")
startup.snapshot("online")
assert(#startup.messages == 2, "Starting offline must recover normally")

local daemon = newService()
daemon.snapshot("online")
daemon.unavailable()
assert(daemon.values.transport.available == false, "Daemon loss must be visible immediately")
daemon.tick(40)
daemon.snapshot("connecting")
daemon.tick(60)
assert(#daemon.messages == 1, "A connecting snapshot must not restart the outage timer")
daemon.snapshot("disconnected")
daemon.tick(90)
assert(#daemon.messages == 1, "Daemon and serial failures in one outage must share one notification")
daemon.snapshot("online")
assert(#daemon.messages == 2, "Recovery after daemon loss must notify once")

local disabled = newService(false)
disabled.snapshot("disconnected")
disabled.tick(120)
assert(#disabled.messages == 0, "Disabled notifications must stay silent")
disabled.configure(true)
disabled.tick(179.999)
assert(#disabled.messages == 0, "Enabling notifications must start a fresh grace period")
disabled.configure(true)
disabled.tick(180)
assert(#disabled.messages == 1, "An unchanged setting must not restart the timer")
disabled.configure(false)
disabled.snapshot("online")
assert(#disabled.messages == 1, "Disabling notifications must clear pending recovery notifications")
disabled.configure(true)
disabled.snapshot("disconnected")
disabled.tick(200)
disabled.configure(false)
disabled.tick(300)
disabled.configure(true)
disabled.tick(359.999)
assert(#disabled.messages == 1, "A cancelled timer must not leak into a later enabled period")
disabled.snapshot("online")

local sensors = newService()
sensors.snapshot("online")
sensors.snapshot("online", {low_charge=true, high_load=true, high_temperature=true})
assert(#sensors.messages == 3, "Charge, load, and temperature warnings must remain immediate")
print("PASS: connection grace period, timer without events, recovery, repeated outages, startup, daemon loss, settings and sensor warnings")
`;
const directory = fs.mkdtempSync(join(tmpdir(), 'sms-gamer-luau-'));
try {
  for (const [name, source] of [['panel', harness], ['service', serviceHarness]]) {
    const script = join(directory, name + '-test.luau');
    fs.writeFileSync(script, source);
    const result = spawnSync(process.env.LUAU || 'luau', [script], {stdio: 'inherit'});
    if (result.error) throw result.error;
    if (result.status !== 0) {
      process.exitCode = result.status ?? 1;
      break;
    }
  }
} finally {
  fs.rmSync(directory, {recursive: true, force: true});
}
