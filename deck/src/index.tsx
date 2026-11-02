/*
 * DeckScreen — Decky Loader frontend.
 * Steam Deck como segunda tela + controle do PC (estilo Wii U GamePad).
 *
 * Built with plain `tsc` (no bundler): React, @decky/ui and the plugin API come
 * from globals that Decky Loader sets up before loading plugins.
 */

const PLUGIN_NAME = "DeckScreen";
const SHORTCUT_NAME = "DeckScreen";
const FLATPAK = "/usr/bin/flatpak";

const React = window.SP_REACT;
const h = React.createElement;
const Fragment = React.Fragment;
const { useEffect, useState } = React;
const {
  ButtonItem, PanelSection, PanelSectionRow, ToggleField, DropdownItem, SliderField,
  TextField, Field, Spinner, staticClasses, Navigation,
} = window.DFL;

const api = window.__DECKY_SECRET_INTERNALS_DO_NOT_USE_OR_YOU_WILL_BE_FIRED_deckyLoaderAPIInit!.connect(1, PLUGIN_NAME);
const call = <T = any,>(method: string, ...args: any[]): Promise<T> => api.call(method, ...args);
const toast = (title: string, body: string) => api.toaster.toast({ title, body });
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

// ------------------------------------------------------------------ types
interface HostInfo {
  id: string; ip: string; port: number; name: string; paired: boolean; mac: string;
  wired: boolean | null; capability: string;
}
interface Found { ip: string; port: number; name: string; version?: string; capability?: string }
interface DeckSettings {
  hosts: HostInfo[]; host: HostInfo | null; active: string; resolution: string; fps: number; bitrate: number;
  preset: string; codec: string; overlay: boolean; audio_on_host: boolean; absolute_touch: boolean;
  motion: "off" | "dsu" | "direct"; shortcut_app_id: number;
}
interface MotionStatus {
  running: boolean; port: number; mode: string; deck_ip: string; clients?: string[];
  imu_active?: boolean; error?: string;
}
interface HostSettings {
  mode: string; position: string; scale: number; orientation: string; profile: string;
  open_app: boolean; close_app: boolean; custom: { rules: string[]; fullscreen: boolean; command: string };
}
interface HostStatus {
  display: { active: boolean; output: string | null; last_error: string | null;
             session: { width: number; height: number; fps: number; mode: string } | null };
  settings: HostSettings;
  diagnostics: { krfb: boolean; kscreen: boolean; wayland: boolean; sunshine_api: boolean; desktop: string; capability: string };
  network?: { mac?: string; wired?: boolean; wol?: string; ip?: string };
  name: string; version: string; capability: string; desktop: string;
}
interface Result<T> { ok: boolean; code?: number; data?: T; error?: string }
interface TestResult {
  ok: boolean; error?: string; tips: string[]; pc_wired: boolean | null;
  rtt: { ok: boolean; min?: number; avg?: number; max?: number; jitter?: number; lost: number; count: number };
  deck: { type: string; band?: string; signal?: number; ssid?: string; rate?: string };
}
interface Check { label: string; status: string; detail: string }

// ------------------------------------------------------------------ Steam shortcut ("DeckScreen" na biblioteca)
function overview(appId: number): any {
  try { return window.appStore?.GetAppOverviewByAppID?.(appId) ?? window.appStore?.m_mapApps?.get(appId) ?? null; }
  catch { return null; }
}

function findShortcut(): number | null {
  let found: number | null = null;
  window.appStore?.m_mapApps?.forEach((ov: any, id: number) => {
    if (found === null && ov?.display_name === SHORTCUT_NAME && ov?.BIsShortcut?.()) found = id;
  });
  return found;
}

function waitDetails(appId: number, pred: (d: any) => boolean, ms = 2500): Promise<boolean> {
  return new Promise((resolve) => {
    let done = false;
    let reg: any;
    const finish = (v: boolean) => { if (!done) { done = true; reg?.unregister?.(); resolve(v); } };
    try {
      reg = SteamClient.Apps.RegisterForAppDetails(appId, (d: any) => { if (d && pred(d)) finish(true); });
    } catch { finish(false); }
    setTimeout(() => finish(false), ms);
  });
}

async function ensureShortcut(saved: number): Promise<number | null> {
  if (saved && overview(saved)) return saved;
  const existing = findShortcut();
  if (existing) return existing;
  const id = await SteamClient.Apps.AddShortcut(SHORTCUT_NAME, FLATPAK, "", "");
  if (typeof id !== "number") return null;
  for (let i = 0; i < 20 && !overview(id); i++) await sleep(150);
  SteamClient.Apps.SetShortcutName(id, SHORTCUT_NAME);
  await waitDetails(id, (d) => d.strDisplayName === SHORTCUT_NAME);
  return id;
}

/** "direct" turns Steam Input off (Moonlight forwards the gyro); everything else keeps Steam's default. */
function applyMotion(appId: number, motion: string) {
  try { SteamClient.Apps.SetThirdPartyControllerConfiguration(appId, motion === "direct" ? 0 : 1); } catch { /* older client */ }
}

async function launchShortcut(appId: number, launchOptions: string): Promise<boolean> {
  SteamClient.Apps.SetAppLaunchOptions(appId, launchOptions);
  await waitDetails(appId, (d) => d.strLaunchOptions === launchOptions);
  const gameId = overview(appId)?.gameid;
  if (!gameId) return false;
  SteamClient.Apps.RunGame(gameId, "", -1, 100);
  return true;
}

function terminateShortcut(appId: number) {
  const gameId = overview(appId)?.gameid;
  if (gameId) { try { SteamClient.Apps.TerminateApp(gameId, false); } catch { /* not running */ } }
}

async function prepareShortcut(): Promise<number | null> {
  const s = await call<DeckSettings>("get_settings");
  const id = await ensureShortcut(s.shortcut_app_id);
  if (id && id !== s.shortcut_app_id) await call("save_settings", { shortcut_app_id: id });
  if (!id) toast("DeckScreen", "Não consegui criar o atalho na Steam. Reinicie a Steam e tente de novo.");
  else applyMotion(id, s.motion);
  return id;
}

// ------------------------------------------------------------------ long tasks (outside React: survive closing the menu)
type PairState = { pin: string; requestId: string; state: string; sunshine?: string } | null;
let pairState: PairState = null;
let wakeState = "";
const listeners = new Set<() => void>();
const notify = () => listeners.forEach((l) => l());
const setPair = (p: PairState) => { pairState = p; notify(); };
const setWake = (w: string) => { wakeState = w; notify(); };

async function runPairing(): Promise<void> {
  const r = await call<{ ok: boolean; pin: string; request_id: string; launch_options: string; error?: string }>("pair_start");
  if (!r.ok) { toast("DeckScreen", r.error || "Falha ao pedir pareamento"); return; }
  setPair({ pin: r.pin, requestId: r.request_id, state: "pending" });
  toast("Aceite no PC", `Confira o PIN ${r.pin} na notificação do PC e clique em Aceitar. A janela do Moonlight que abrir agora é só do pareamento e fecha sozinha.`);

  const appId = await prepareShortcut();
  if (appId) await launchShortcut(appId, r.launch_options);   // Moonlight: pair <ip> --pin XXXX

  const deadline = Date.now() + 3 * 60 * 1000;
  let accepted = false;
  while (Date.now() < deadline) {
    await sleep(1500);
    const st = await call<{ ok: boolean; state?: string; sunshine?: string }>("pair_status", r.request_id);
    if (!st.ok) continue;
    setPair({ pin: r.pin, requestId: r.request_id, state: st.state || "pending", sunshine: st.sunshine });
    if (st.state === "denied") { toast("DeckScreen", "O PC recusou o pareamento."); break; }
    if (st.state === "approved" && !accepted) { accepted = true; toast("DeckScreen", "PC aceitou! Finalizando com o Moonlight…"); }
    if (accepted && st.sunshine && st.sunshine !== "waiting") {
      toast("DeckScreen", st.sunshine === "ok"
        ? "Pareado! Agora toque em Conectar no menu do DeckScreen (⋯ → Decky)."
        : "O Moonlight não confirmou o PIN. Se ele já estava pareado, pode ignorar; senão tente de novo.");
      await sleep(2500);
      if (appId) {
        terminateShortcut(appId);
        // From now on the "DeckScreen" tile in the library connects (instead of pairing again).
        const opts = await call<string>("stream_launch_options");
        if (opts) {
          SteamClient.Apps.SetAppLaunchOptions(appId, opts);
          await waitDetails(appId, (d) => d.strLaunchOptions === opts);
        }
      }
      break;
    }
  }
  if (!accepted && Date.now() >= deadline) toast("DeckScreen", "Tempo esgotado. Tente parear de novo.");
  setPair(null);
}

async function wakeAndWait(): Promise<boolean> {
  setWake("Acordando o PC…");
  const r = await call<{ ok: boolean; error?: string }>("wake_host", 90);
  setWake("");
  if (!r.ok) toast("DeckScreen", r.error || "O PC não acordou.");
  return r.ok;
}

async function connect(online: boolean): Promise<void> {
  if (!online) {
    const s = await call<DeckSettings>("get_settings");
    if (!s.host?.mac) { toast("DeckScreen", "O PC está desligado ou fora da rede."); return; }
    if (!(await wakeAndWait())) return;
    await sleep(3000); // give Sunshine a moment after resume
  }
  const appId = await prepareShortcut();
  if (!appId) return;
  const opts = await call<string>("stream_launch_options");
  if (await launchShortcut(appId, opts)) Navigation.CloseSideMenus();
  else toast("DeckScreen", "Não consegui abrir o Moonlight.");
}

// ------------------------------------------------------------------ UI helpers
const Row = (p: { children?: any }) => h(PanelSectionRow, null, p.children);
const Note = (p: { children?: any; color?: string }) =>
  h("div", { style: { fontSize: "12px", lineHeight: "16px", color: p.color || "#b8bcbf", padding: "2px 0 6px", whiteSpace: "pre-line" } }, p.children);
const Btn = (p: { label: string; onClick: () => any; disabled?: boolean; desc?: string }) =>
  h(Row, null, h(ButtonItem, { layout: "below", disabled: p.disabled, description: p.desc, onClick: p.onClick }, p.label));
const Toggle = (p: { label: string; desc?: string; checked: boolean; onChange: (v: boolean) => any }) =>
  h(Row, null, h(ToggleField, { label: p.label, description: p.desc, checked: p.checked, onChange: p.onChange }));
const Drop = (p: { label: string; options: { data: any; label: string }[]; value: any; onChange: (v: any) => any }) =>
  h(Row, null, h(DropdownItem, { label: p.label, rgOptions: p.options, selectedOption: p.value, onChange: (o: any) => p.onChange(o.data) }));

const Icon = () =>
  h("svg", { viewBox: "0 0 24 24", width: "1em", height: "1em", fill: "currentColor" },
    h("path", { d: "M2 5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2h-6v2h3v2H7v-2h3v-2H4a2 2 0 0 1-2-2V5zm2 0v9h16V5H4z" }),
    h("rect", { x: 9, y: 7, width: 6, height: 5, rx: 1 }));

const opts = (pairs: [any, string][]) => pairs.map(([data, label]) => ({ data, label }));
const MODE_OPTS = opts([["extend", "Estender (2ª tela)"], ["deck_only", "Só no Deck (desliga os monitores)"], ["mirror", "Espelhar a tela principal"]]);
const POS_OPTS = opts([["right", "À direita"], ["left", "À esquerda"], ["above", "Acima"], ["below", "Abaixo"]]);
const ORIENT_OPTS = opts([["landscape", "Horizontal"], ["portrait", "Vertical (experimental)"], ["portrait_flipped", "Vertical invertido (experimental)"]]);
const SCALE_OPTS = [1, 1.25, 1.5, 1.75, 2].map((v) => ({ data: v, label: `${Math.round(v * 100)}%` }));
const PROFILE_OPTS = opts([["desktop", "Área de trabalho"], ["cemu", "Wii U (Cemu)"], ["custom", "Personalizado"]]);
const PRESET_OPTS = opts([["low_latency", "Baixa latência"], ["balanced", "Equilibrado"], ["quality", "Qualidade (mais suave)"]]);
const CODEC_OPTS = opts([["auto", "Automático"], ["HEVC", "HEVC (recomendado)"], ["H.264", "H.264"]]);
const RES_OPTS = opts([["1280x800", "1280×800 (nativa do Deck)"], ["1920x1080", "1920×1080"], ["1920x1200", "1920×1200"], ["2560x1600", "2560×1600"]]);
const MOTION_OPTS = opts([["off", "Desligado"], ["dsu", "Para emuladores (mantém o Steam Input)"], ["direct", "Direto pelo Moonlight (desliga o Steam Input)"]]);
const FPS_OPTS = opts([[30, "30"], [40, "40"], [60, "60"], [90, "90 (Deck OLED)"]]);

function describeMotion(m: MotionStatus | null): string {
  if (!m) return "";
  if (m.mode === "direct") return "O Steam Input fica desligado no atalho DeckScreen: os trackpads deixam de funcionar como mouse.";
  if (m.mode !== "dsu") return "";
  if (!m.running) return `Não consegui abrir a porta UDP ${m.port}.`;
  const lines = [`No emulador do PC (Cemu, Dolphin, Ryujinx…): fonte de movimento DSU/cemuhook em ${m.deck_ip || "IP do Deck"}:${m.port}.`];
  if (m.error) lines.push("⚠ " + m.error);
  if (m.clients && m.clients.length) {
    lines.push(`Conectado: ${m.clients.join(", ")}`);
    if (!m.imu_active) lines.push("⚠ A Steam não está mandando o giroscópio: no layout do controle do DeckScreen, use o giroscópio em alguma ação (ex.: \"como mouse\" ativado por um botão que você não usa).");
  }
  return lines.join("\n");
}

function describeTest(t: TestResult): string {
  const lines: string[] = [];
  if (t.rtt.ok) lines.push(`Ida e volta: média ${t.rtt.avg} ms (mín ${t.rtt.min}, máx ${t.rtt.max}, variação ${t.rtt.jitter})`);
  if (t.rtt.lost) lines.push(`Perdidos: ${t.rtt.lost}/${t.rtt.count}`);
  if (t.deck.type === "wifi") lines.push(`Deck: Wi-Fi ${t.deck.band} · sinal ${t.deck.signal}% · ${t.deck.rate || ""}`);
  else if (t.deck.type === "ethernet") lines.push("Deck: cabo (dock)");
  if (t.pc_wired !== null && t.pc_wired !== undefined) lines.push(`PC: ${t.pc_wired ? "cabo" : "Wi-Fi"}`);
  return lines.concat(t.tips.map((x) => "• " + x)).join("\n");
}

// ------------------------------------------------------------------ main panel
function Content() {
  const [s, setS] = useState(null as DeckSettings | null);
  const [status, setStatus] = useState(null as Result<HostStatus> | null);
  const [moonlight, setMoonlight] = useState(null as boolean | null);
  const [busy, setBusy] = useState("");
  const [found, setFound] = useState(null as Found[] | null);
  const [ip, setIp] = useState("");
  const [adding, setAdding] = useState(false);
  const [, force] = useState(0);
  const [rulesText, setRulesText] = useState(null as string | null);
  const [cmdText, setCmdText] = useState(null as string | null);
  const [test, setTest] = useState(null as TestResult | null);
  const [checks, setChecks] = useState(null as Check[] | null);
  const [showVideo, setShowVideo] = useState(false);
  const [motion, setMotion] = useState(null as MotionStatus | null);

  const refresh = async () => {
    const ns = await call<DeckSettings>("get_settings");
    setS(ns);
    if (ns.host && ns.host.paired) setStatus(await call<Result<HostStatus>>("host_status"));
    else setStatus(null);
    setMotion(ns.motion === "off" ? null : await call<MotionStatus>("motion_status"));
  };

  useEffect(() => {
    refresh();
    call<{ installed: boolean }>("moonlight_status").then((m) => setMoonlight(m.installed));
    const l = () => force((n: number) => n + 1);
    listeners.add(l);
    const t = setInterval(refresh, 4000);
    return () => { clearInterval(t); listeners.delete(l); };
  }, []);

  const withBusy = async (label: string, fn: () => Promise<void>) => {
    setBusy(label);
    try { await fn(); } finally { setBusy(""); }
  };

  if (!s) return h(PanelSection, null, h(Row, null, h(Spinner, { style: { height: "32px" } })));

  const updateDeck = async (patch: any) => {
    const ns = await call<DeckSettings>("save_settings", patch);
    setS(ns);
    if ("motion" in patch) {
      if (ns.shortcut_app_id) applyMotion(ns.shortcut_app_id, ns.motion);
      setMotion(ns.motion === "off" ? null : await call<MotionStatus>("motion_status"));
    }
  };

  // ---------- Moonlight
  const moonlightSection = moonlight === false && h(PanelSection, { title: "Moonlight" },
    h(Row, null, h(Note, null, "O DeckScreen usa o Moonlight para receber a tela do PC. Ele ainda não está instalado.")),
    h(Btn, {
      label: busy === "moonlight" ? "Instalando…" : "Instalar Moonlight", disabled: !!busy,
      onClick: () => withBusy("moonlight", async () => {
        toast("DeckScreen", "Instalando o Moonlight (Flathub)… pode levar alguns minutos.");
        const r = await call<{ ok: boolean; error?: string }>("install_moonlight");
        if (r.ok) { setMoonlight(true); toast("DeckScreen", "Moonlight instalado!"); }
        else toast("Falha ao instalar o Moonlight", r.error || "erro desconhecido");
      }),
    }));

  // ---------- choose / add a PC
  if (!s.host || adding) {
    const addHost = (addr: string, port: number) => withBusy("add", async () => {
      const r = await call<{ ok: boolean; error?: string }>("add_host", addr, port);
      if (!r.ok) toast("DeckScreen", r.error || "Erro");
      else { setAdding(false); setFound(null); setIp(""); await refresh(); }
    });
    return h(Fragment, null,
      moonlightSection,
      h(PanelSection, { title: s.hosts.length ? "Adicionar PC" : "1. Escolha o PC" },
        h(Row, null, h(Note, null, "Instale o DeckScreen no PC (AppImage ou install.sh) e deixe-o ligado na mesma rede.")),
        h(Btn, {
          label: busy === "discover" ? "Procurando…" : "Procurar PC na rede", disabled: !!busy,
          onClick: () => withBusy("discover", async () => setFound(await call<Found[]>("discover"))),
        }),
        found && found.length === 0 && h(Row, null, h(Note, null, "Nenhum PC encontrado. Digite o IP abaixo.")),
        ...((found as Found[] | null) || []).map((f: Found) => h(Btn, {
          label: `Usar ${f.name}`, desc: `${f.ip}${f.capability === "mirror" ? " · só espelha" : ""}`, disabled: !!busy,
          onClick: () => addHost(f.ip, f.port),
        })),
        h(Row, null, h(TextField, { label: "IP do PC", value: ip, placeholder: "192.168.0.10", onChange: (e: any) => setIp(e.target.value) })),
        h(Btn, { label: "Usar este IP", disabled: !!busy || !ip.trim(), onClick: () => addHost(ip.trim(), 47555) }),
        s.hosts.length > 0 && h(Btn, { label: "Cancelar", onClick: () => setAdding(false) })));
  }

  const host = s.host;
  const pcPicker = s.hosts.length > 1 && h(Drop, {
    label: "PC", options: s.hosts.map((x) => ({ data: x.id, label: x.name })), value: s.active,
    onChange: async (id: string) => { await call("select_host", id); setStatus(null); setTest(null); setChecks(null); await refresh(); },
  });

  // ---------- pair
  if (!host.paired) {
    const pair = pairState;
    return h(Fragment, null,
      moonlightSection,
      h(PanelSection, { title: "2. Parear" },
        pcPicker,
        h(Row, null, h(Field, { label: host.name, description: host.ip })),
        pair
          ? h(Fragment, null,
              h(Row, null, h("div", { style: { fontSize: "40px", fontWeight: "bold", textAlign: "center", letterSpacing: "8px" } }, pair.pin)),
              h(Row, null, h(Note, null, pair.state === "approved"
                ? "PC aceitou. Finalizando o pareamento do Moonlight…"
                : "No PC, clique em “Aceitar” na notificação (confira se o PIN é este). Sem notificação? Use o widget DeckScreen na bandeja ou rode: deckscreen-host accept")))
          : h(Btn, { label: "Parear com este PC", disabled: !!busy || moonlight === false, onClick: () => { runPairing().then(refresh); } }),
        h(Btn, { label: "Adicionar outro PC", disabled: !!pair, onClick: () => setAdding(true) }),
        h(Btn, {
          label: "Remover este PC", disabled: !!busy || !!pair,
          onClick: () => withBusy("forget", async () => { await call("forget_host", host.id); await refresh(); }),
        })));
  }

  // ---------- connected
  const st = status?.ok ? status.data! : null;
  const hs = st?.settings;
  const active = !!st?.display.active;
  const online = !!st;
  const capability = st?.capability || host.capability;
  const updateHost = async (patch: any) => {
    const r = await call<Result<HostSettings>>("host_update_settings", patch);
    if (!r.ok) toast("DeckScreen", r.error || "Erro ao salvar no PC");
    await refresh();
  };

  const warnings: string[] = [];
  if (st) {
    if (capability === "mirror") {
      warnings.push(st.desktop === "gnome"
        ? "No GNOME o Deck espelha a tela por enquanto. Virar monitor extra depende de uma opção proposta ao Sunshine oficial; quando ela sair, o DeckScreen ativa sozinho."
        : `Este PC (${st.desktop}) só espelha a tela: para virar monitor extra use KDE Plasma Wayland, GNOME ou Hyprland.`);
    }
    if (st.diagnostics.desktop === "kde" && !st.diagnostics.krfb) warnings.push("PC sem o pacote krfb (tela virtual).");
    if (!st.diagnostics.sunshine_api) warnings.push("O DeckScreen do PC não acessa o Sunshine (parado ou senha errada: rode 'deckscreen-host install').");
    if (st.display.last_error) warnings.push(st.display.last_error);
  }
  const statusLine = status === null ? "Verificando…"
    : st ? (active && st.display.session
        ? `Tela do Deck ativa · ${st.display.session.width}×${st.display.session.height} @ ${st.display.session.fps}`
        : `Online · ${host.ip}${host.wired === false ? " · PC no Wi-Fi" : ""}`)
    : (wakeState || status.error || "Offline");

  return h(Fragment, null,
    moonlightSection,
    h(PanelSection, { title: "PC" },
      pcPicker,
      h(Row, null, h(Field, { label: st ? st.name : host.name, description: statusLine })),
      ...warnings.map((w) => h(Row, null, h(Note, { color: "#ffb84d" }, `⚠ ${w}`))),
      h(Btn, {
        label: busy === "connect" ? (wakeState || "Abrindo…") : !online && host.mac ? "Ligar o PC e conectar" : active ? "Reabrir tela" : "Conectar",
        disabled: !!busy || moonlight === false || (!online && !host.mac),
        onClick: () => withBusy("connect", () => connect(online)),
      }),
      !online && host.mac && h(Btn, {
        label: busy === "wake" ? "Acordando…" : "Só ligar o PC (Wake-on-LAN)", disabled: !!busy,
        onClick: () => withBusy("wake", async () => { if (await wakeAndWait()) await refresh(); }),
      }),
      online && h(Btn, {
        label: "Encerrar", disabled: !!busy,
        onClick: () => withBusy("stop", async () => {
          await call("host_stop");
          if (s.shortcut_app_id) terminateShortcut(s.shortcut_app_id);
          await refresh();
        }),
      })),

    hs && capability !== "mirror" && h(PanelSection, { title: "Tela no PC" },
      h(Drop, { label: "Modo", options: MODE_OPTS, value: hs.mode, onChange: (v) => updateHost({ mode: v }) }),
      hs.mode === "extend" && h(Drop, { label: "Posição do Deck", options: POS_OPTS, value: hs.position, onChange: (v) => updateHost({ position: v }) }),
      hs.mode !== "mirror" && h(Drop, { label: "Escala", options: SCALE_OPTS, value: hs.scale, onChange: (v) => updateHost({ scale: v }) }),
      h(Drop, { label: "Orientação", options: ORIENT_OPTS, value: hs.orientation, onChange: (v) => updateHost({ orientation: v }) }),
      h(Row, null, h(Note, null, st?.desktop === "gnome"
        ? "Mudanças valem na próxima conexão. No GNOME, trocar de/para Espelhar reinicia o Sunshine no PC. Na 1ª conexão, aceite no PC o \"Compartilhar tela\" escolhendo \"Monitor virtual\" e marcando \"Lembrar\"."
        : "Mudanças valem na próxima conexão."))),

    hs && h(PanelSection, { title: "Perfil" },
      h(Drop, { label: "Perfil", options: PROFILE_OPTS, value: hs.profile, onChange: (v) => updateHost({ profile: v }) }),
      hs.profile === "cemu" && h(Row, null, h(Note, null,
        "Modo Wii U: a janela “GamePad View” do Cemu vai para o Deck em tela cheia. No Cemu, ative Options → Separate GamePad view.")),
      hs.profile !== "desktop" && st?.desktop === "gnome" && h(Row, null, h(Note, { color: "#ffb84d" },
        "No GNOME as janelas não podem ser movidas automaticamente: leve a janela para o Deck com Super+Shift+→ (ou arraste).")),
      hs.profile !== "desktop" && h(Toggle, { label: "Abrir o app ao conectar", checked: hs.open_app, onChange: (v) => updateHost({ open_app: v }) }),
      hs.profile !== "desktop" && h(Toggle, { label: "Fechar o app ao desconectar", checked: hs.close_app, onChange: (v) => updateHost({ close_app: v }) }),
      hs.profile === "custom" && h(Fragment, null,
        h(Row, null, h(TextField, {
          label: "Janelas para o Deck (títulos, separados por vírgula)", value: rulesText ?? hs.custom.rules.join(", "),
          onChange: (e: any) => setRulesText(e.target.value),
        })),
        h(Row, null, h(TextField, {
          label: "Comando do app (opcional)", value: cmdText ?? hs.custom.command, placeholder: "melonDS",
          onChange: (e: any) => setCmdText(e.target.value),
        })),
        h(Toggle, { label: "Janelas em tela cheia", checked: hs.custom.fullscreen, onChange: (v) => updateHost({ custom: { fullscreen: v } }) }),
        h(Btn, {
          label: "Salvar perfil",
          onClick: async () => {
            await updateHost({ custom: {
              rules: (rulesText ?? hs.custom.rules.join(", ")).split(",").map((x: string) => x.trim()).filter(Boolean),
              command: cmdText ?? hs.custom.command,
            } });
            setRulesText(null); setCmdText(null);
            toast("DeckScreen", "Perfil salvo no PC.");
          },
        }))),

    h(PanelSection, { title: "Deck" },
      h(Drop, { label: "Latência", options: PRESET_OPTS, value: s.preset, onChange: (v) => updateDeck({ preset: v }) }),
      h(Toggle, { label: "Mostrar medidor de atraso", desc: "Sobreposição do Moonlight com o tempo de rede e decodificação.", checked: s.overlay, onChange: (v) => updateDeck({ overlay: v }) }),
      h(Toggle, { label: "Toque = clique direto", desc: "Tocar na tela clica onde você tocou.", checked: s.absolute_touch, onChange: (v) => updateDeck({ absolute_touch: v }) }),
      h(Toggle, { label: "Som no PC", desc: "Desligado: o som sai no Deck.", checked: s.audio_on_host, onChange: (v) => updateDeck({ audio_on_host: v }) }),
      h(Drop, { label: "Giroscópio", options: MOTION_OPTS, value: s.motion, onChange: (v) => updateDeck({ motion: v }) }),
      motion && h(Row, null, h(Note, null, describeMotion(motion))),
      h(Toggle, { label: "Opções de vídeo", checked: showVideo, onChange: setShowVideo }),
      showVideo && h(Fragment, null,
        h(Drop, { label: "Codec", options: CODEC_OPTS, value: s.codec, onChange: (v) => updateDeck({ codec: v }) }),
        h(Drop, { label: "Resolução", options: RES_OPTS, value: s.resolution, onChange: (v) => updateDeck({ resolution: v }) }),
        h(Drop, { label: "FPS", options: FPS_OPTS, value: s.fps, onChange: (v) => updateDeck({ fps: v }) }),
        h(Row, null, h(SliderField, {
          label: "Bitrate", value: Math.round(s.bitrate / 1000), min: 5, max: 80, step: 5, showValue: true, valueSuffix: " Mbps",
          onChange: (v: number) => updateDeck({ bitrate: v * 1000 }),
        })))),

    h(PanelSection, { title: "Rede e diagnóstico" },
      h(Btn, {
        label: busy === "test" ? "Testando…" : "Testar conexão", disabled: !!busy,
        onClick: () => withBusy("test", async () => setTest(await call<TestResult>("test_connection"))),
      }),
      test && h(Row, null, h(Note, null, test.ok ? describeTest(test) : test.error)),
      h(Btn, {
        label: busy === "doctor" ? "Verificando…" : "Diagnóstico do PC", disabled: !!busy || !online,
        onClick: () => withBusy("doctor", async () => {
          const r = await call<Result<{ checks: Check[] }>>("host_doctor");
          if (r.ok && r.data) setChecks(r.data.checks); else toast("DeckScreen", r.error || "Falhou");
        }),
      }),
      checks && h(Row, null, h(Note, null, checks.map((c: Check) =>
        `${c.status === "ok" ? "✔" : c.status === "warn" ? "!" : "✘"} ${c.label}: ${c.detail}`).join("\n"))),
      h(Btn, { label: "Adicionar outro PC", onClick: () => setAdding(true) }),
      h(Btn, {
        label: "Esquecer este PC", disabled: !!busy,
        onClick: () => withBusy("forget", async () => { await call("forget_host", host.id); setStatus(null); await refresh(); }),
      })));
}

export default function () {
  return {
    name: PLUGIN_NAME,
    titleView: h("div", { className: staticClasses.Title }, "DeckScreen"),
    content: h(Content, null),
    icon: h(Icon, null),
    onDismount() { listeners.clear(); },
  };
}
