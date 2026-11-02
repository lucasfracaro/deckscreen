// Smoke test for the Decky frontend: fake React + fake @decky/ui + fake Steam client.
// Walks: find PC -> pair -> connect -> settings/profile/gyro -> test -> doctor -> stop -> offline wake.
// Run from deck/: node ../tests/frontend_smoke.mjs
import assert from "node:assert/strict";
import { pathToFileURL } from "node:url";
import path from "node:path";

const log = [];

// ---------- fake React (enough for one stateful component)
let hooks = [], hookIdx = 0, effects = [], dirty = false;
const React = {
  Fragment: "Fragment",
  createElement(type, props, ...children) {
    return { type, props: { ...(props || {}), children: children.flat(Infinity) } };
  },
  useState(init) {
    const i = hookIdx++;
    if (!(i in hooks)) hooks[i] = init;
    const set = (v) => { hooks[i] = typeof v === "function" ? v(hooks[i]) : v; dirty = true; };
    return [hooks[i], set];
  },
  useEffect(fn) {
    const i = hookIdx++;
    if (!(i in hooks)) { hooks[i] = true; effects.push(fn); }
  },
};
function expand(node) {
  if (node === null || node === undefined || node === false || node === true || node === "") return [];
  if (Array.isArray(node)) return node.flatMap(expand);
  if (typeof node !== "object") return [String(node)];
  if (typeof node.type === "function") return expand(node.type(node.props));
  return [{ ...node, kids: node.props.children.flatMap(expand) }];
}
const text = (n) => typeof n === "string" ? n : n.kids.map(text).join(" ");
function all(tree, pred, acc = []) {
  for (const n of tree) { if (typeof n === "object") { if (pred(n)) acc.push(n); all(n.kids, pred, acc); } }
  return acc;
}

// ---------- fake @decky/ui
const comp = (name) => name;
globalThis.window = {
  SP_REACT: React,
  DFL: {
    ButtonItem: comp("ButtonItem"), PanelSection: comp("PanelSection"), PanelSectionRow: comp("Row"),
    ToggleField: comp("ToggleField"), DropdownItem: comp("DropdownItem"), SliderField: comp("SliderField"),
    TextField: comp("TextField"), Field: comp("Field"), Spinner: comp("Spinner"),
    staticClasses: { Title: "t" }, Navigation: { CloseSideMenus: () => log.push("close-menu") },
  },
};

// ---------- fake backend (mirrors main.py responses)
const be = {
  hosts: {}, active: "", shortcut_app_id: 0, fps: 60, preset: "low_latency", gyro: false, polls: 0, online: true,
  hostSettings: { mode: "extend", position: "right", scale: 1, orientation: "landscape", profile: "desktop",
    open_app: true, close_app: false, custom: { rules: [], fullscreen: true, command: "" } },
};
const hostList = () => Object.entries(be.hosts).map(([id, h]) => ({ id, ...h }));
const settings = () => ({
  hosts: hostList(), host: hostList().find((h) => h.id === be.active) || null, active: be.active,
  resolution: "1280x800", fps: be.fps, bitrate: 20000, preset: be.preset, codec: "auto", overlay: false,
  audio_on_host: false, absolute_touch: true, gyro: be.gyro, shortcut_app_id: be.shortcut_app_id,
});
const backend = {
  get_settings: () => settings(),
  save_settings: (p) => {
    if (p.shortcut_app_id) be.shortcut_app_id = p.shortcut_app_id;
    for (const k of ["fps", "preset", "gyro"]) if (k in p) be[k] = p[k];
    return settings();
  },
  moonlight_status: () => ({ installed: true }),
  discover: () => [{ ip: "192.168.0.10", port: 47555, name: "cachyos-pc", capability: "virtual" }],
  add_host: (ip, port) => { be.hosts["cachyos-pc"] = { ip, port, name: "cachyos-pc", paired: false, mac: "", wired: null, capability: "virtual" }; be.active = "cachyos-pc"; return { ok: true, id: "cachyos-pc" }; },
  select_host: (id) => { be.active = id; return true; },
  pair_start: () => ({ ok: true, pin: "1234", request_id: "r1", launch_options: "run com.moonlight_stream.Moonlight pair 192.168.0.10 --pin 1234" }),
  pair_status: () => {
    be.polls++;
    const ok = be.polls >= 2;
    if (ok) Object.assign(be.hosts["cachyos-pc"], { paired: true, mac: "aa:bb:cc:dd:ee:ff", wired: true });
    return { ok: true, state: ok ? "approved" : "pending", sunshine: ok ? "ok" : "waiting" };
  },
  host_status: () => be.online
    ? { ok: true, code: 200, data: { name: "cachyos-pc", version: "2.0.0", capability: "virtual", desktop: "kde", settings: be.hostSettings,
        display: { active: false, output: null, last_error: null, session: null },
        diagnostics: { krfb: true, kscreen: true, wayland: true, sunshine_api: true, desktop: "kde", capability: "virtual" } } }
    : { ok: false, code: 0, error: "PC não respondeu" },
  host_update_settings: (p) => {
    for (const [k, v] of Object.entries(p)) {
      if (k === "custom") Object.assign(be.hostSettings.custom, v); else be.hostSettings[k] = v;
    }
    return { ok: true, data: be.hostSettings };
  },
  stream_launch_options: () => "run com.moonlight_stream.Moonlight stream 192.168.0.10 DeckScreen --fps 60",
  host_stop: () => { log.push("host_stop"); return { ok: true }; },
  host_doctor: () => ({ ok: true, data: { checks: [{ label: "krfb", status: "ok", detail: "/usr/bin/krfb-virtualmonitor" }] } }),
  test_connection: () => ({ ok: true, tips: ["Rede ótima para streaming. 👍"], pc_wired: true,
    rtt: { ok: true, min: 1.1, avg: 1.9, max: 3.0, jitter: 0.4, lost: 0, count: 20 },
    deck: { type: "wifi", band: "5 GHz", signal: 82, rate: "866 Mbit/s" } }),
  wake_host: () => { log.push("wake"); be.online = true; return { ok: true }; },
  forget_host: () => { be.hosts = {}; be.active = ""; return true; },
};
window.__DECKY_SECRET_INTERNALS_DO_NOT_USE_OR_YOU_WILL_BE_FIRED_deckyLoaderAPIInit = {
  connect: (v, name) => {
    assert.equal(name, "DeckScreen");
    return {
      call: async (m, ...a) => { log.push(`call:${m}`); assert.ok(backend[m], `missing backend ${m}`); return backend[m](...a); },
      toaster: { toast: (t) => log.push(`toast:${t.body}`) },
    };
  },
};

// ---------- fake Steam client
const apps = new Map();
const details = new Map();
window.appStore = { m_mapApps: apps, GetAppOverviewByAppID: (id) => apps.get(id) ?? null };
globalThis.SteamClient = {
  Apps: {
    AddShortcut: async (name, exe) => { const id = 3000000001; apps.set(id, { display_name: "flatpak", gameid: "1234567", BIsShortcut: () => true }); details.set(id, { strDisplayName: "flatpak", strLaunchOptions: "" }); log.push(`add:${exe}`); return id; },
    SetShortcutName: (id, n) => { apps.get(id).display_name = n; details.get(id).strDisplayName = n; },
    SetAppLaunchOptions: (id, o) => { details.get(id).strLaunchOptions = o; log.push(`opts:${o}`); },
    RegisterForAppDetails: (id, cb) => { setTimeout(() => cb(details.get(id)), 5); return { unregister() {} }; },
    SetThirdPartyControllerConfiguration: (id, v) => log.push(`steaminput:${v}`),
    RunGame: (gid) => log.push(`run:${gid}`),
    TerminateApp: (gid) => log.push(`terminate:${gid}`),
  },
};

// ---------- load plugin
const mod = await import(pathToFileURL(path.resolve("dist/index.js")).href);
const plugin = mod.default();
assert.equal(plugin.name, "DeckScreen");

const tick = (ms = 30) => new Promise((r) => setTimeout(r, ms));
async function render() {
  for (let i = 0; i < 10; i++) {
    hookIdx = 0; dirty = false;
    const tree = expand(plugin.content);
    const fx = effects; effects = [];
    fx.forEach((f) => f());
    await tick();
    if (!dirty && !fx.length) return tree;
  }
  hookIdx = 0;
  return expand(plugin.content);
}
const buttons = (tree) => all(tree, (n) => n.type === "ButtonItem");
async function click(label) {
  const tree = await render();
  const b = buttons(tree).find((n) => text(n).includes(label));
  assert.ok(b, `button "${label}" not found. Have: ${buttons(tree).map(text).join(" | ")}`);
  assert.ok(!b.props.disabled, `button "${label}" disabled`);
  await b.props.onClick();
  await tick(60);
  return render();
}
const find = (tree, type, label) => all(tree, (n) => n.type === type && String(n.props.label).includes(label))[0];

// choose PC
let tree = await render();
assert.ok(buttons(tree).some((b) => text(b).includes("Procurar PC")), "choose-PC visible");
tree = await click("Procurar PC");
tree = await click("Usar cachyos-pc");
// pair
assert.ok(buttons(tree).some((b) => text(b).includes("Parear com este PC")), "pair visible");
await click("Parear com este PC");
await tick(200);
tree = await render();
assert.ok(text({ kids: tree }).includes("1234"), "PIN shown while pairing");
await tick(5200);
tree = await render();
assert.ok(log.includes("add:/usr/bin/flatpak"), "shortcut created");
assert.ok(log.some((l) => l.startsWith("opts:run com.moonlight_stream.Moonlight pair")), "pair launch options");
assert.ok(log.includes("terminate:1234567"), "moonlight pair window closed after success");
assert.ok(log.includes("steaminput:1"), "Steam Input left on by default");
// connect
tree = await click("Conectar");
assert.ok(log.some((l) => l.includes("stream 192.168.0.10 DeckScreen")), "stream options set");
assert.ok(log.includes("close-menu"), "menu closed after connect");
// host settings + profile
await find(tree, "DropdownItem", "Modo").props.onChange({ data: "mirror" });
assert.equal(be.hostSettings.mode, "mirror");
tree = await render();
assert.ok(!find(tree, "DropdownItem", "Posição"), "position hidden when mirroring");
await find(tree, "DropdownItem", "Perfil").props.onChange({ data: "custom" });
tree = await render();
find(tree, "TextField", "Janelas").props.onChange({ target: { value: "Secondary, Top Screen" } });
tree = await render();
tree = await click("Salvar perfil");
assert.deepEqual(be.hostSettings.custom.rules, ["Secondary", "Top Screen"]);
// deck: gyro + preset
await find(tree, "ToggleField", "Giroscópio").props.onChange(true);
assert.ok(log.includes("steaminput:0"), "gyro turns Steam Input off");
await find(tree, "DropdownItem", "Latência").props.onChange({ data: "quality" });
assert.equal(be.preset, "quality");
// tests
tree = await click("Testar conexão");
assert.ok(text({ kids: tree }).includes("média 1.9 ms"), "test result shown");
tree = await click("Diagnóstico do PC");
assert.ok(text({ kids: tree }).includes("✔ krfb"), "doctor shown");
tree = await click("Encerrar");
assert.ok(log.includes("host_stop"));
// offline -> wake
be.online = false;
tree = await render();
await tick(4200);
tree = await render();
tree = await click("Ligar o PC e conectar");
await tick(3500);
assert.ok(log.includes("wake"), "wake-on-lan sent");
console.log("frontend smoke test: OK (" + log.length + " events)");
process.exit(0);
