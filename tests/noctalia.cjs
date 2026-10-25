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
const directory = fs.mkdtempSync(join(tmpdir(), 'sms-gamer-luau-'));
try {
  const script = join(directory, 'panel-test.luau');
  fs.writeFileSync(script, harness);
  const result = spawnSync(process.env.LUAU || 'luau', [script], {stdio: 'inherit'});
  if (result.error) throw result.error;
  process.exitCode = result.status ?? 1;
} finally {
  fs.rmSync(directory, {recursive: true, force: true});
}
