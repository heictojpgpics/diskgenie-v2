/**
 * Mock command registry (DEV/TEST ONLY): implements the full Tauri
 * command surface against the synthetic tree so the UI runs in a plain
 * browser. Used by Playwright/agent-browser screenshot passes; NEVER
 * bundled into production (gated on isTauri()=false + DEV in main.tsx).
 */
import { emitMockEvent, setMockBackend } from "../lib/ipc";
import { normalizeKey, isValidKeyShape } from "../lib/licenseKey";
import { buildLayout, encodeLayout } from "./layouts";
import { CATEGORY_COLORS, CATEGORY_LABELS, MockTree } from "./tree";

const MB = 1024 * 1024;
const GB = 1024 * MB;
const DAY = 86400;
const NOW = Math.floor(Date.now() / 1000);

let tree = new MockTree();
let scanning = false;
let scanTicker: number | null = null;
/** The sticky last-done record (mirrors the Rust DoneRecord — the
 *  store's reconcile reads it back through get_status). */
let lastDone: { generation: number; stats: [number, number, number, number] | null; error: string | null } | null = null;
let monitorTicker: number | null = null;
let monitorSession = 0;
/** Duplicates run state (mirrors the Rust ctl): a live run emits
 *  `dupes-progress` on a ~200 ms ticker and can be cancelled — the
 *  pending promise rejects with "cancelled" exactly like the engine. */
let dupesCancelGen = 0;
let dupesTicker: number | null = null;
/** Mock mirror of the Rust app-lifetime dupes status (the page-switch
 * fix): a running scan survives view unmounts, `dupes_status` serves
 * the live snapshot / sticky result to a remounted view. */
let dupesRunning = false;
let dupesLastProgress: Record<string, unknown> | null = null;
let dupesResult: Record<string, unknown> | null = null;
let dupesError: string | null = null;
/** The scoped folder of the current run / sticky result (session 15
 * "Duplicates here" parity — null = whole tree). */
let dupesScopePath: string | null = null;
const snapshots: { id: string; root: string; takenAt: number; total: number; folders: number; map: Map<string, number> }[] = [];
/** License mock (mirrors the Rust LicenseStatusView): starts
 * unlicensed; `license_sim_set` (the CI tour hook) flips it. */
let license: Record<string, unknown> = {
  posture: "unlicensed", isPro: false, tier: "",
  customerName: "", customerEmail: "", licenseExpiresAt: 0,
  graceDaysLeft: 0, purchaseUrl: "https://diskgenie.app/pricing",
  simulated: false,
};
const PRO_SIM = {
  posture: "pro", isPro: true, tier: "lifetime",
  customerName: "Alex Morgan", customerEmail: "alex@diskgenie.app",
  licenseExpiresAt: 0, graceDaysLeft: 0,
  purchaseUrl: "https://diskgenie.app/pricing", simulated: true,
};
/** The Rust gate marker (require_licensed) — the ipc layer
 * intercepts it and opens the activation flow. */
const GATE_MSG = "ACTIVATION_REQUIRED — Activate DiskGenie Pro to use this.";
function licenseGate(): void {
  const p = String(license.posture);
  if (p === "pro" || p === "grace") return;
  throw GATE_MSG;
}
let lastScanRoot = 0;
/** The flip-cache keys whose scans COMPLETED (session 14 — Rust
 * `AppState.tree_cache` parity): a `start_scan` for a key already in
 * the set restores instantly instead of running the fake ticker.
 * The mock has ONE tree object, so the "cached tree" is the same
 * tree — the RESTORE FLOW (instant done, no scanning state, stats
 * from the standing tree) is the part under test. */
const scannedKeys = new Set<string>();

/** Rust `tree_cache_key` parity: This-PC spellings collapse; Windows
 * family lowercases + one trailing separator (the mock speaks
 * Windows paths). */
function cacheKey(target: string): string {
  const trimmed = target.trim().toLowerCase();
  if (trimmed === "thispc" || trimmed === "this pc") {
    return "thispc";
  }
  return `${target.replace(/[\\/]+$/, "").toLowerCase()}\\`;
}
void 0;

const fmtPath = (id: number): string => tree.pathOf(id);

function nodeDetails(id: number): Record<string, unknown> {
  const n = tree.nodes[id];
  const agg = tree.aggregate(id);
  const parent = tree.nodes[n.parent];
  const stats = tree.stats(id);
  // children_sorted parity (Rust): size-desc, then top-10.
  const largest = [...n.children]
    .sort((a, b) => (tree.nodes[b].onDisk || 0) - (tree.nodes[a].onDisk || 0))
    .slice(0, 10)
    .map((c) => {
      const cn = tree.nodes[c];
      return { id: c, name: cn.name, size: cn.onDisk || cn.logical, isDir: cn.isDir };
    });
  const domCat = tree.dominantCategory(id);
  // Drive roots show as "Disk" (parity with Rust compute_details — a
  // path-shaped X:\ check; "This PC" and real folders stay "Folder").
  const path = fmtPath(id);
  const isDrive = n.isDir && /^[A-Za-z]:\\?$/.test(path);
  return {
    id,
    name: n.name,
    isDir: n.isDir,
    kind: n.isDir ? (isDrive ? "Disk" : "Folder") : CATEGORY_LABELS[n.category],
    kindColor: CATEGORY_COLORS[n.isDir ? domCat : n.category],
    path,
    size: stats.onDisk,
    shareOfScan: stats.onDisk / (tree.nodes[lastScanRoot].onDisk || 1),
    logical: stats.logical,
    overhead: Math.max(0, stats.onDisk - stats.logical),
    savings: Math.max(0, stats.logical - stats.onDisk),
    files: agg.files,
    folders: agg.folders,
    ofParent: parent ? stats.onDisk / (parent.onDisk || 1) : 1,
    modified: n.modified,
    created: n.created,
    isCloud: n.cloud,
    isProtected: n.protected,
    largest,
  };
}

function startScan(_target: string, _turbo: boolean, force = false): { generation: number; restored: boolean; stats: [number, number, number, number] | null } {
  const key = cacheKey(_target);
  // The instant restore (Rust parity — the drive-flip fix): a target
  // whose scan completed before restores WITHOUT the ticker: bump the
  // generation, emit scan-done synchronously (the store drops it as
  // stale and adopts from the restored flag — the same timing as the
  // real backend), return the stats. `force` (the Rescan button)
  // purges the key so the fresh scan replaces it.
  if (force) {
    scannedKeys.delete(key);
  } else if (scannedKeys.has(key)) {
    if (scanTicker !== null) window.clearInterval(scanTicker);
    scanning = false;
    const generation = tree.generation + 1;
    tree.generation = generation;
    lastScanRoot = 0;
    const st = tree.stats(0);
    lastDone = {
      generation,
      stats: [st.logical, st.onDisk, st.files, st.folders],
      error: null,
    };
    emitMockEvent("scan-done", {
      generation,
      stats: [st.logical, st.onDisk, st.files, st.folders],
      error: null,
    });
    return { generation, restored: true, stats: [st.logical, st.onDisk, st.files, st.folders] };
  }
  if (scanTicker !== null) window.clearInterval(scanTicker);
  scanning = true;
  const generation = tree.generation + 1;
  tree.generation = generation;
  lastScanRoot = 0;
  // Dev-only race harness: ?fastscan=1 completes the scan on the FIRST
  // SYNCHRONOUS tick (inside start_scan, before the invoke resolves) —
  // reproducing the tiny-tree timing the CI screenshot tours exposed
  // (scan-done fired while the store still held the old generation).
  const fastScan = new URLSearchParams(location.search).get("fastscan") === "1";
  const totalFiles = fastScan ? 1 : 1600;
  let files = 0;
  const started = performance.now();
  const tick = () => {
    files = Math.min(totalFiles, files + (fastScan ? totalFiles : Math.round(totalFiles / 16)));
    emitMockEvent("scan-progress", {
      generation,
      progress: {
        files,
        folders: Math.round(files / 6),
        bytes: Math.round(files * 41.2 * MB),
        currentPath: fmtPath(tree.nodes[Math.floor(Math.random() * tree.nodes.length)].id),
        denied: 3,
        deniedSamples: tree.denied.samples,
      },
    });
    if (files >= totalFiles) {
      if (scanTicker !== null) window.clearInterval(scanTicker);
      scanTicker = null;
      void started;
      scanning = false;
      const st = tree.stats(0);
      lastDone = {
        generation,
        stats: [st.logical, st.onDisk, st.files, st.folders],
        error: null,
      };
      // The completion FILES the key (Rust swap_tree parity: a tree
      // enters the flip cache when a scan DONE swaps it in — a
      // cancelled scan caches nothing).
      scannedKeys.add(key);
      emitMockEvent("scan-done", {
        generation,
        stats: [st.logical, st.onDisk, st.files, st.folders],
        error: null,
      });
    }
  };
  tick();
  scanTicker = window.setInterval(tick, 170);
  return { generation, restored: false, stats: null };
}

let monitorBase: Record<string, number> = {};

function monitorSample(): Record<string, unknown> {
  const dt = 2000;
  const cpu = 14 + Math.random() * 30;
  const total = 32 * GB;
  const avail = 9.5 * GB + Math.random() * GB;
  const procNames = [
    "explorer.exe", "chrome.exe", "Code.exe", "cargo.exe", "rust-analyzer.exe",
    "Discord.exe", "Spotify.exe", "SearchIndexer.exe", "MsMpEng.exe", "dwm.exe",
    "System", "svchost.exe", "python.exe", "node.exe", "VRChat.exe",
  ];
  const procs = procNames.map((name, i) => ({
    pid: 1000 + i * 37,
    name,
    cpuPct: Math.max(0, (cpu / procNames.length) * (2 - i / procNames.length) * (0.4 + Math.random())),
    ws: Math.round((620 - i * 36) * MB * (0.6 + Math.random() * 0.8)),
  }));
  monitorBase.in = (monitorBase.in ?? 0) + 2.2 * MB + Math.random() * MB;
  monitorBase.out = (monitorBase.out ?? 0) + 0.4 * MB + Math.random() * 0.2 * MB;
  return {
    dtMs: dt,
    cpuUserPct: cpu * 0.62,
    cpuSystemPct: cpu * 0.38,
    cpuTotalPct: cpu,
    threads: 1840 + Math.round(Math.random() * 300),
    processes: 214 + Math.round(Math.random() * 30),
    memTotal: total,
    memAvailable: avail,
    kernelPaged: 380 * MB,
    kernelNonpaged: 96 * MB,
    systemCache: 2.1 * GB,
    commitTotal: 26.4 * GB,
    commitLimit: 48.6 * GB,
    compressed: 2.8 * GB,
    netDownBps: 2.2 * MB,
    netUpBps: 0.4 * MB,
    sessionIn: monitorBase.in,
    sessionOut: monitorBase.out,
    volumes: [
      { root: "C:\\", label: "Local Disk", total: 512 * GB, free: 61.4 * GB },
      { root: "D:\\", label: "Games", total: 2048 * GB, free: 812 * GB },
      { root: "E:\\", label: "REMOVABLE", total: 64 * GB, free: 12 * GB },
    ],
    procs,
    totalProcs: 214,
  };
}

const DUPES = [
  {
    id: 1,
    files: [
      { path: "C:\\Users\\dev\\Pictures\\Camera Roll\\photo-402.jpg", nodeId: 4021, modified: NOW - 2 * DAY },
      { path: "C:\\Users\\dev\\Pictures\\photo-402.jpg", nodeId: 4022, modified: NOW - 31 * DAY },
      { path: "C:\\Users\\dev\\Downloads\\photo-402 (1).jpg", nodeId: 4023, modified: NOW - 9 * DAY },
    ],
    size: 24 * MB,
    count: 3,
    wasted: 48 * MB,
  },
  {
    id: 2,
    files: [
      { path: "C:\\Users\\dev\\Documents\\report-233.pdf", nodeId: 2331, modified: NOW - 120 * DAY },
      { path: "C:\\Users\\dev\\Documents\\Work\\report-233.pdf", nodeId: 2332, modified: NOW - 3 * DAY },
    ],
    size: 8.4 * MB,
    count: 2,
    wasted: 8.4 * MB,
  },
  {
    id: 3,
    files: [
      { path: "C:\\Users\\dev\\Videos\\render-102.mp4", nodeId: 1021, modified: NOW - 45 * DAY },
      { path: "C:\\Users\\dev\\Videos\\render-102 copy.mp4", nodeId: 1022, modified: NOW - 45 * DAY },
      { path: "C:\\Users\\dev\\Downloads\\render-102.mp4", nodeId: 1023, modified: NOW - 12 * DAY },
      { path: "D:\\Backups\\render-102.mp4", nodeId: 1024, modified: NOW - 1 * DAY },
    ],
    size: 1.1 * GB,
    count: 4,
    wasted: 3.3 * GB,
  },
];
const APPS = [
  {
    id: "JetBrains RustRover 2026.1", name: "RustRover", publisher: "JetBrains s.r.o.", version: "2026.1.2",
    source: "registry", installLocation: "C:\\Program Files\\JetBrains\\RustRover", uninstallString: "C:\\Program Files\\JetBrains\\RustRover\\Uninstall.exe",
    quietUninstallString: "", packageFullName: "", lastUsed: NOW - 2 * DAY, icon: "",
    bundleSize: 1.4 * GB, leftovers: [{ label: "Roaming AppData", paths: [{ path: "C:\\Users\\dev\\AppData\\Roaming\\JetBrains\\RustRover2026.1", size: 480 * MB }], size: 480 * MB }],
    total: 1.4 * GB + 480 * MB,
  },
  {
    id: "Google Chrome", name: "Google Chrome", publisher: "Google LLC", version: "141.0.7390.65",
    source: "registry", installLocation: "C:\\Program Files\\Google\\Chrome", uninstallString: "C:\\Program Files\\Google\\Chrome\\Application\\141.0.7390.65\\Installer\\setup.exe --uninstall",
    quietUninstallString: "", packageFullName: "", lastUsed: NOW - 30, icon: "",
    bundleSize: 612 * MB, leftovers: [], total: 612 * MB,
  },
  {
    id: "Microsoft VS Code", name: "Visual Studio Code", publisher: "Microsoft Corporation", version: "1.102.0",
    source: "registry", installLocation: "C:\\Program Files\\Microsoft VS Code", uninstallString: "C:\\Program Files\\Microsoft VS Code\\Unins000.exe",
    quietUninstallString: "", packageFullName: "", lastUsed: NOW - 60, icon: "",
    bundleSize: 420 * MB, leftovers: [
      { label: "Roaming AppData", paths: [{ path: "C:\\Users\\dev\\AppData\\Roaming\\Code", size: 210 * MB }], size: 210 * MB },
      { label: "Local AppData", paths: [{ path: "C:\\Users\\dev\\AppData\\Local\\Programs\\VSCode", size: 90 * MB }], size: 90 * MB },
    ],
    total: 420 * MB + 300 * MB,
  },
  {
    id: "Spotify", name: "Spotify", publisher: "Spotify AB", version: "1.2.53",
    source: "msix", installLocation: "", uninstallString: "", quietUninstallString: "",
    packageFullName: "SpotifyAB.SpotifyMusic_1.2.53_x86__zpdnekdrzrea0", lastUsed: NOW - 6 * 3600, icon: "",
    bundleSize: 780 * MB, leftovers: [{ label: "Store package data", paths: [{ path: "C:\\Users\\dev\\AppData\\Local\\Packages\\SpotifyAB.SpotifyMusic", size: 1.6 * GB }], size: 1.6 * GB }],
    total: 780 * MB + 1.6 * GB,
  },
];

type Cmd = (args: Record<string, unknown>) => unknown;

// ── Quick Wins engine (port of core quickwins.rs, Windows table) ───
const CATEGORY_CAP = 400; // core quickwins::CATEGORY_CAP
const QW_USERPROFILE = "C:\\Users\\dev";
const QW_LOCALAPPDATA = "C:\\Users\\dev\\AppData\\Local";

interface QwCat {
  id: string;
  title: string;
  icon: string;
  items: number[];
  size: number;
  reviewOnly: boolean;
  extra: string | null;
}

export function resolveQuickWins(): QwCat[] {
  const eqCi = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();
  const findByPath = (p: string): number => {
    for (let i = 1; i < tree.nodes.length; i++) {
      if (eqCi(tree.pathOf(i), p)) return i;
    }
    return 0; // Rust: unwrap_or(tree.root)
  };
  // match_pattern parity: final-segment matches under root; '*' = one segment.
  const matchPattern = (root: string, segments: string[]): number[] => {
    let current = [findByPath(root)];
    for (const seg of segments) {
      const next: number[] = [];
      for (const cur of current) {
        for (const c of tree.nodes[cur].children) {
          const cn = tree.nodes[c];
          if (seg === "*" || eqCi(cn.name, seg)) next.push(c);
        }
      }
      current = next;
      if (current.length === 0) break;
    }
    return current.filter((id) => id !== 0);
  };
  // find_named parity (any depth, dirs-only option).
  const findNamed = (name: string, dirsOnly: boolean): number[] => {
    const out: number[] = [];
    for (let i = 1; i < tree.nodes.length; i++) {
      const n = tree.nodes[i];
      if ((!dirsOnly || n.isDir) && eqCi(n.name, name)) out.push(i);
    }
    return out;
  };
  const isDescendantOf = (desc: number, anc: number): boolean => {
    let cur = tree.nodes[desc].parent;
    while (cur > 0) {
      if (cur === anc) return true;
      cur = tree.nodes[cur].parent;
    }
    return false;
  };
  const nodeSize = (id: number): number => tree.nodes[id].onDisk || 0;
  // push_cat parity: drop items nested inside a same-category match,
  // cap at 400, sum on-disk, drop empty categories.
  const pushCat = (
    id: string, title: string, icon: string, reviewOnly: boolean,
    extra: string | null, items: number[],
  ): QwCat | null => {
    const filtered: number[] = [];
    for (const it of items) {
      if (filtered.some((f) => isDescendantOf(it, f))) continue;
      filtered.push(it);
      if (filtered.length >= CATEGORY_CAP) break;
    }
    if (filtered.length === 0) return null;
    return {
      id, title, icon, items: filtered,
      size: filtered.reduce((a, x) => a + nodeSize(x), 0),
      reviewOnly, extra,
    };
  };

  const out: QwCat[] = [];
  // Pattern categories (Windows table, verbatim locations + env roots).
  const browsers = ["Google\\Chrome", "Microsoft\\Edge", "BraveSoftware\\Brave-Browser"];
  const cacheSegs = ["Cache", "Code Cache", "GPUCache"];
  const patternCats: [string, string, string, string, string[]][] = [
    ["downloads", "Downloads", "download", QW_USERPROFILE, ["Downloads"]],
    ["temp_caches", "Temp & caches", "temp", QW_LOCALAPPDATA, ["Temp"]],
    ["temp_caches", "Temp & caches", "temp", QW_LOCALAPPDATA, ["Microsoft", "Windows", "INetCache"]],
    ["temp_caches", "Temp & caches", "temp", QW_LOCALAPPDATA, ["CrashDumps"]],
    ["temp_caches", "Temp & caches", "temp", QW_LOCALAPPDATA, ["D3DSCache"]],
    ["temp_caches", "Temp & caches", "temp", QW_LOCALAPPDATA, ["Microsoft", "Windows", "WER"]],
    ...browsers.flatMap((b) =>
      cacheSegs.map((cache) => [
        "browser_caches", "Browser caches", "browser", QW_LOCALAPPDATA,
        [...b.split("\\"), "*", cache],
      ] as [string, string, string, string, string[]]),
    ),
    ["browser_caches", "Browser caches", "browser", QW_LOCALAPPDATA, ["Mozilla", "Firefox", "Profiles", "*", "cache2"]],
    ["dev_caches", "Developer caches", "code", QW_USERPROFILE, [".nuget", "packages"]],
    ["dev_caches", "Developer caches", "code", QW_USERPROFILE, [".cargo", "registry"]],
    ["dev_caches", "Developer caches", "code", QW_USERPROFILE, [".gradle", "caches"]],
    ["dev_caches", "Developer caches", "code", QW_LOCALAPPDATA, ["npm-cache"]],
    ["dev_caches", "Developer caches", "code", QW_LOCALAPPDATA, ["pip", "Cache"]],
    ["dev_caches", "Developer caches", "code", QW_LOCALAPPDATA, ["pnpm", "store"]],
    ["dev_caches", "Developer caches", "code", QW_LOCALAPPDATA, ["Yarn", "Cache"]],
    ["android_emulators", "Android emulators", "phone", QW_USERPROFILE, [".android", "avd"]],
  ];
  const buckets = new Map<string, number[]>();
  for (const [cat, , , root, segs] of patternCats) {
    for (const id of matchPattern(root, segs)) {
      const b = buckets.get(cat);
      if (b) b.push(id);
      else buckets.set(cat, [id]);
    }
  }
  const catMeta: [string, string, string][] = [
    ["downloads", "Downloads", "download"],
    ["temp_caches", "Temp & caches", "temp"],
    ["browser_caches", "Browser caches", "browser"],
    ["dev_caches", "Developer caches", "code"],
    ["android_emulators", "Android emulators", "phone"],
  ];
  for (const [id, title, icon] of catMeta) {
    const items = buckets.get(id);
    if (items) {
      const cat = pushCat(id, title, icon, false, null, items);
      if (cat) out.push(cat);
    }
  }

  // node_modules: any depth, dirs named node_modules.
  {
    const cat = pushCat("node_modules", "node_modules", "code", false, null, findNamed("node_modules", true));
    if (cat) out.push(cat);
  }
  // Build artifacts: unconditional names + sibling-ruled target/bin/obj.
  {
    const BUILD_ARTIFACT_NAMES = [
      "build", ".build", "dist", ".next", ".nuxt", ".turbo", ".parcel-cache",
      ".terraform", "__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache",
      ".tox", ".gradle",
    ];
    const ba: number[] = [];
    for (let i = 1; i < tree.nodes.length; i++) {
      const n = tree.nodes[i];
      if (!n.isDir) continue;
      if (BUILD_ARTIFACT_NAMES.some((f) => eqCi(n.name, f))) { ba.push(i); continue; }
      if (n.parent <= 0) continue;
      const siblings = tree.nodes[n.parent].children.map((c) => tree.nodes[c].name);
      if (eqCi(n.name, "target")) {
        if (siblings.some((s) => s === "Cargo.toml" || s === "pom.xml")) ba.push(i);
      } else if (eqCi(n.name, "bin") || eqCi(n.name, "obj")) {
        if (siblings.some((s) => s === "project.json" || /\.(csproj|vcxproj)$/i.test(s))) ba.push(i);
      }
    }
    const cat = pushCat("build_artifacts", "Build artifacts", "hammer", false, null, ba);
    if (cat) out.push(cat);
  }
  // Large media: video/audio/image files >= 10 MB (cats 0/1/2).
  {
    const lm: number[] = [];
    for (let i = 1; i < tree.nodes.length; i++) {
      const n = tree.nodes[i];
      if (!n.isDir && n.logical >= 10 * MB && (n.category === 0 || n.category === 1 || n.category === 2)) lm.push(i);
    }
    const cat = pushCat("large_media", "Large media", "video", false, null, lm);
    if (cat) out.push(cat);
  }
  // VM disks (review-only): VM_DISK_ROOTS + any .vhdx >= 1 GB.
  {
    const vmRoots: [string, string[]][] = [
      [QW_LOCALAPPDATA, ["Packages", "*", "LocalState"]],
      [QW_LOCALAPPDATA, ["Docker"]],
      [QW_USERPROFILE, ["VirtualBox VMs"]],
    ];
    const vm: number[] = [];
    for (const [root, segs] of vmRoots) vm.push(...matchPattern(root, segs));
    for (let i = 1; i < tree.nodes.length; i++) {
      const n = tree.nodes[i];
      if (!n.isDir && n.logical >= GB && /\.vhdx$/i.test(n.name)) vm.push(i);
    }
    const cat = pushCat("vm_disks", "VM disks", "server", true, null, vm);
    if (cat) out.push(cat);
  }
  // Previous Windows install (review-only + storagesense link).
  {
    const cat = pushCat("windows_old", "Previous Windows install", "clock", true, "ms-settings:storagesense", findNamed("Windows.old", true));
    if (cat) out.push(cat);
  }
  out.sort((a, b) => b.size - a.size);
  return out;
}

/** Items for one category (the quick_win_items command body, exported
 *  for the parity tests). Mirrors the Rust command: re-resolve, cap,
 *  {id, path, size: on_disk}. */
export function quickWinItems(categoryId: string): { id: number; path: string; size: number }[] {
  const cat = resolveQuickWins().find((c) => c.id === categoryId);
  if (!cat) return [];
  return cat.items
    .slice(0, CATEGORY_CAP)
    .map((id) => ({ id, path: fmtPath(id), size: tree.nodes[id].onDisk || 0 }));
}

/** Row shape for the UI (items stripped — ids are engine-internal). */
export function stripItems(c: QwCat): Record<string, unknown> {
  return {
    id: c.id,
    title: c.title,
    icon: c.icon,
    count: c.items.length,
    size: c.size,
    reviewOnly: c.reviewOnly,
    extra: c.extra,
    biggestMatch: c.items[0],
  };
}

/** The shared cleanup-commit body (mock parity with
 * `recycle::shell_delete` + `plan_commit`): shortest-first, nested and
 * duplicate paths absorbed, protected items refused with a reason, the
 * tree surgered without a rescan, the `cleanup-committed` event fired.
 * The mock has no real shell — the Windows CI integration test owns
 * the real IFileOperation proof. */
function commitShellDeleteMock(
  a: Record<string, unknown>,
  _mode: "recycle" | "permanent",
): Record<string, unknown> {
  licenseGate();
  const items = (a.items ?? []) as { id: number; path: string; size: number }[];
  // plan_commit: sort shortest-first, absorb nested + same-path rows.
  const sorted = [...items].sort((x, y) =>
    x.path.length === y.path.length ? x.path.localeCompare(y.path) : x.path.length - y.path.length,
  );
  const keep: { id: number; path: string; size: number }[] = [];
  const absorbed: { id: number; path: string }[] = [];
  for (const it of sorted) {
    const hit = keep.find(
      (k) =>
        k.path.toLowerCase() === it.path.toLowerCase() ||
        (it.path.length > k.path.length &&
          it.path.toLowerCase().startsWith(k.path.toLowerCase()) &&
          it.path[k.path.length] === "\\"),
    );
    if (hit) absorbed.push({ id: it.id, path: it.path });
    else keep.push(it);
  }
  const failed: { path: string; reason: string }[] = [];
  const removed: { id: number; path: string }[] = [];
  const removedIds: number[] = [];
  for (const it of keep) {
    if (it.id > 0 && tree.nodes[it.id]?.protected) {
      failed.push({ path: it.path, reason: "Windows manages this item" });
    } else {
      if (it.id > 0 && it.id < tree.nodes.length) removedIds.push(it.id);
      removed.push({ id: it.id, path: it.path });
    }
  }
  tree.removeSubtrees(removedIds);
  const st = tree.stats(0);
  // Navigation fixup parity: a commit that removes the ON-SCREEN
  // folder lands the view on a survivor (the real engine walks the
  // parent chain; the mock's root is the universal survivor).
  const askedFolder = Number(a.currentFolder ?? 0);
  const landed = removedIds.includes(askedFolder) ? 0 : askedFolder;
  const askedSelection = a.selectedNode == null ? null : Number(a.selectedNode);
  const result = {
    generation: tree.generation,
    // Removed roots + absorbed rows (nested) — every staged row whose
    // path moved clears from the queue (Rust parity).
    trashed: [
      ...removed.map((r) => ({ path: r.path, alreadyGone: false, nested: false })),
      ...absorbed.map((r) => ({ path: r.path, alreadyGone: false, nested: true })),
    ],
    failed,
    stats: [st.logical, st.onDisk, st.files, st.folders],
    currentFolder: landed,
    selectedNode: askedSelection != null && removedIds.includes(askedSelection) ? null : askedSelection,
  };
  window.setTimeout(() => {
    emitMockEvent("cleanup-committed", result);
  }, 60);
  return result;
}

/** The command registry (exported for the vitest parity tests — the
 * browser install wraps it with the async IPC boundary). */
export const commands: Record<string, Cmd> = {
  // ── scan lifecycle ────────────────────────────────────────────────
  get_dev_hooks: () => ({
    scan: new URLSearchParams(location.search).get("scan") ?? null,
    mode: new URLSearchParams(location.search).get("mode") ?? null,
    turbo: false,
    verify: false,
    tour: new URLSearchParams(location.search).get("tour") === "1",
    // Session-17 parity: the tour PROGRAM (ui/license/bench) + the
    // real-key hook + the window spec — the mock parses them from the
    // URL the same way (mock tests drive the license tour with
    // ?tourMode=license&tourLicenseKey=DB…).
    tourMode: new URLSearchParams(location.search).get("tourMode") ?? null,
    tourLicenseKey: new URLSearchParams(location.search).get("tourLicenseKey") ?? null,
    window: new URLSearchParams(location.search).get("window") ?? null,
  }),
  get_status: () => ({
    generation: tree.generation,
    scanning,
    hasTree: true,
    progress: { files: 1600, folders: 266, bytes: 66 * GB, currentPath: "", denied: 3, deniedSamples: tree.denied.samples },
    lastDone,
  }),
  start_scan: (a) => {
    licenseGate();
    return startScan(String(a.target), false, a.force === true);
  },
  start_scan_turbo: (a) => {
    licenseGate();
    // Rust parity: the turbo command still returns the bare generation
    // (the ScanStart object is the STANDARD command's shape — turbo
    // never restores; extract the number the store's turbo path types).
    return startScan(String(a.target), true).generation;
  },
  cancel_scan: () => {
    // Mirrors the Rust cooperative cancel: stop the ticker, flip the
    // flag. The store reverts client-side (tree stays as-is).
    if (scanTicker !== null) {
      window.clearInterval(scanTicker);
      scanTicker = null;
    }
    scanning = false;
    return true;
  },
  resolve_path: (a: Record<string, unknown>) => {
    // Mirrors the Rust resolve_path: None (null) when the generation is
    // stale or the path is outside the scanned tree. The mock tree has
    // a This-PC synthetic root above the drive ("Local Disk (C:)"),
    // which pathOf() skips — resolve must skip it the same way.
    if (Number(a.generation) !== tree.generation) return null;
    const target = String(a.path).replace(/[\/]+$/, "").replace(/\//g, "\\");
    const root = "C:\\";
    const lower = target.toLowerCase();
    if (lower !== root && !lower.startsWith(root.toLowerCase())) return null;
    // The drive node: the root child whose pathOf() is exactly "C:\".
    const drive = tree.nodes[0].children.find((c) => fmtPath(c) === root);
    if (drive === undefined) return null;
    if (lower === root) return drive;
    const segs = target
      .slice(root.length)
      .split("\\")
      .filter((s) => s.length > 0);
    let cur = drive;
    for (const seg of segs) {
      const kids = tree.nodes[cur].children;
      const hit = kids.find((k) => tree.nodes[k].name.toLowerCase() === seg.toLowerCase());
      if (hit === undefined) return null;
      cur = hit;
    }
    return cur;
  },
  get_drive_chips: () => [{ letter: "C:", target: "C:\\" }, { letter: "D:", target: "D:\\" }],
  get_home_path: () => "C:\\Users\\dev",
  // Rust `disk_storage(path)` parity (session 15): the probe follows
  // the VIEW — a C: path returns the C: volume (letter-suffixed on
  // the multi-drive shape like the real multi-root trees), a D: path
  // the Games volume, and anything unmatched ("This PC", null) the
  // multi-volume aggregate. The old one-shape answer is exactly the
  // bug the owner reported: the sidebar never changed on a flip.
  disk_storage: (a) => {
    const path = typeof a?.path === "string" ? a.path : null;
    if (path && /^c:/i.test(path)) {
      return { label: "Local Disk (C:)", total: 512 * GB, used: 450.6 * GB, free: 61.4 * GB, usedPct: 0.880 };
    }
    if (path && /^d:/i.test(path)) {
      const total = 2048 * GB;
      const free = 812 * GB;
      const used = total - free;
      return { label: "Games (D:)", total, used, free, usedPct: used / total };
    }
    if (path) {
      // Unmatched view paths ("This PC") get the aggregate, exactly
      // like the real command's multi-root branch.
      const total = 512 * GB + 2048 * GB;
      const used = 450.6 * GB + (2048 * GB - 812 * GB);
      return { label: "This PC", total, used, free: total - used, usedPct: used / total };
    }
    return { label: "Local Disk (C:)", total: 512 * GB, used: 450.6 * GB, free: 61.4 * GB, usedPct: 0.880 };
  },
  is_elevated: () => true,
  // Parity with the Rust flow: the real command either exits (success)
  // or emits `admin-restart-failed`. The mock can't restart, so it
  // takes the failure branch — the elevation toast (with the UAC
  // shield) is exercisable in dev/demo exactly as on Windows.
  restart_as_admin: () => {
    window.setTimeout(() => {
      emitMockEvent("admin-restart-failed", "Elevation was declined or failed (shell error 5).");
    }, 350);
    return null;
  },
  open_recycle_bin: () => null,
  open_url: () => null,
  copy_path: (a) => {
    console.info("[mock] copy_path", a);
    return null;
  },
  open_node: (a) => {
    // Parity note: the real command launches the default app; the mock
    // (a browser page with nothing to launch) says so on the toast
    // surface instead, so the button's contract is verifiable live.
    console.info("[mock] open_node", a);
    window.dispatchEvent(
      new CustomEvent("db-toast", {
        detail: { text: "Opened with the default app (mock).", icon: "check" },
      }),
    );
    return null;
  },
  reveal_in_explorer: (a) => {
    console.info("[mock] reveal_in_explorer", a);
    return null;
  },

  // ── sidebar data ──────────────────────────────────────────────────
  quick_wins: () => resolveQuickWins().map(stripItems),
  quick_win_items: (a) => quickWinItems(String(a.categoryId)),
  file_types: () => {
    const sizes = new Array(9).fill(0);
    for (let i = 1; i < tree.nodes.length; i++) {
      const n = tree.nodes[i];
      if (!n.isDir) sizes[n.category] += n.logical;
    }
    return sizes
      .map((size, i) => ({ label: CATEGORY_LABELS[i], color: CATEGORY_COLORS[i], size }))
      .filter((s) => s.size > 0)
      .sort((a, b) => b.size - a.size);
  },

  // ── explore data ──────────────────────────────────────────────────
  get_folder_view: (a) => {
    const node = Number(a.node);
    const filter = (a.filter as string | null) ?? "";
    const n = tree.nodes[node];
    const folders = n.children
      .filter((c) => tree.nodes[c].isDir)
      .filter((c) => !filter || tree.nodes[c].name.toLowerCase().includes(filter.toLowerCase()))
      .slice(0, 24)
      .map((c) => {
        const cn = tree.nodes[c];
        const cats = tree.topCategories(c, 3);
        return {
          id: c,
          name: cn.name,
          size: cn.onDisk || cn.logical,
          itemCount: (tree.aggregate(c).files + tree.aggregate(c).folders),
          fileCount: tree.aggregate(c).files,
          folderCount: tree.aggregate(c).folders,
          categories: cats.map((x) => ({ color: CATEGORY_COLORS[x.category], label: CATEGORY_LABELS[x.category], size: x.size })),
          modified: cn.modified,
          protected: cn.protected,
        };
      });
    const files = n.children
      .filter((c) => !tree.nodes[c].isDir)
      .filter((c) => !filter || tree.nodes[c].name.toLowerCase().includes(filter.toLowerCase()))
      .slice(0, 12)
      .map((c) => {
        const cn = tree.nodes[c];
        return {
          id: c,
          name: cn.name,
          size: cn.onDisk || cn.logical,
          category: CATEGORY_LABELS[cn.category],
          categoryColor: CATEGORY_COLORS[cn.category],
          modified: cn.modified,
          protected: cn.protected,
          cloud: cn.cloud,
        };
      });
    const st = tree.stats(node);
    return {
      generation: tree.generation,
      node,
      name: node === 0 ? "This PC" : n.name,
      size: st.onDisk,
      fileCount: st.files,
      folderCount: st.folders,
      folders,
      files,
      filesCapped: files.length >= 12,
    };
  },
  node_details: (a) => nodeDetails(Number(a.id)),
  hover_details: (a) => {
    const id = Number(a.id);
    const n = tree.nodes[id];
    return {
      id,
      name: n.name,
      isDir: n.isDir,
      size: n.onDisk || n.logical,
      shareOfScan: (n.onDisk || n.logical) / (tree.nodes[lastScanRoot || 0].onDisk || 1),
      fileCount: n.isDir ? tree.aggregate(id).files : 1,
      isCloud: n.cloud,
      isProtected: n.protected,
      category: n.isDir ? "Folder" : CATEGORY_LABELS[n.category],
      categoryColor: CATEGORY_COLORS[n.isDir ? 8 : n.category],
    };
  },
  top_sizes: (a) => {
    // Parity with commands/explore.rs top_sizes: in-folder ranks are the
    // PRE-filter positions in children_sorted order (gaps survive the
    // filter); anywhere-scopes rank candidates 1..N then RE-RANK
    // contiguously after the filter drops rows. Sizes are on-disk.
    const node = Number(a.node);
    const scope = String(a.scope);
    const filter = (a.filter as string | null) ?? "";
    const st = tree.stats(node);
    const fileCount = (n: { fileCount?: number }) => n.fileCount ?? 0;
    const rows: { rank: number; id: number; name: string; parentPath: string; size: number; kind: string; isDir: boolean; share: number; color: number }[] = [];
    if (scope === "in-folder") {
      // children_sorted: on-disk desc, id asc tie-break (rollup.rs).
      const kids = [...tree.nodes[node].children].sort(
        (x, y) => tree.nodes[y].onDisk - tree.nodes[x].onDisk || x - y,
      );
      kids.forEach((c, i) => {
        const cn = tree.nodes[c] as typeof tree.nodes[number] & { fileCount?: number };
        if (cn.onDisk === 0 || tree.removed.has(c)) return; // push_row early-return
        if (filter && !cn.name.toLowerCase().includes(filter.toLowerCase())) return;
        rows.push({
          rank: i + 1, id: c, name: cn.name, parentPath: "", size: cn.onDisk,
          kind: cn.isDir ? `${fileCount(cn)} files` : CATEGORY_LABELS[cn.category], isDir: cn.isDir,
          share: cn.onDisk / (st.onDisk || 1), color: cn.isDir ? CATEGORY_COLORS[8] : CATEGORY_COLORS[cn.category],
        });
      });
    } else {
      const wantDirs = scope === "folders-anywhere";
      const candidates: { id: number; onDisk: number }[] = [];
      for (const nid of tree.allDescendants(node)) {
        const n = tree.nodes[nid];
        if (n.isDir !== wantDirs || tree.removed.has(nid) || n.onDisk === 0) continue;
        if (wantDirs && nid === node) continue; // the scope folder frames, never a row
        candidates.push({ id: nid, onDisk: n.onDisk });
      }
      candidates.sort((x, y) => y.onDisk - x.onDisk);
      let rank = 0;
      for (const cand of candidates) {
        if (rank >= 200) break; // TOP_CAP
        const n = tree.nodes[cand.id] as typeof tree.nodes[number] & { fileCount?: number };
        if (tree.removed.has(cand.id) || n.onDisk === 0) continue;
        if (filter && !n.name.toLowerCase().includes(filter.toLowerCase())) continue;
        rank += 1;
        rows.push({
          rank, id: cand.id, name: n.name, parentPath: fmtPath(n.parent), size: n.onDisk,
          kind: n.isDir ? `${fileCount(n)} files` : CATEGORY_LABELS[n.category], isDir: n.isDir,
          share: n.onDisk / (st.onDisk || 1), color: n.isDir ? CATEGORY_COLORS[8] : CATEGORY_COLORS[n.category],
        });
      }
      // Re-rank after filter drops (explore.rs does this for anywhere-scopes).
      rows.forEach((r, i) => { r.rank = i + 1; });
    }
    return { generation: tree.generation, node, scope, rows, total: st.onDisk };
  },
  age_map: (a) => {
    const node = Number(a.node);
    const buckets = new Array(6).fill(0);
    const now = NOW;
    const monthMap = new Map<string, number>();
    const big: { id: number; name: string; path: string; logical: number; onDisk: number; modified: number; protected: boolean }[] = [];
    for (const d of tree.allDescendants(node)) {
      const n = tree.nodes[d];
      if (n.isDir) continue;
      const b = ageBucketOf(n.modified, now);
      // Mirror the real engine: age analysis is allocated-byte based, so
      // sparse/compressed files have the same proportions in mock and Tauri.
      buckets[b] += n.onDisk;
      const dt = new Date(n.modified * 1000);
      const key = `${dt.getUTCFullYear()}-${dt.getUTCMonth()}`;
      monthMap.set(key, (monthMap.get(key) ?? 0) + n.onDisk);
      if (n.logical >= 40 * MB && now - n.modified > 365 * DAY && !n.cloud) {
        big.push({ id: d, name: n.name, path: fmtPath(d), logical: n.logical, onDisk: n.onDisk, modified: n.modified, protected: n.protected });
      }
    }
    big.sort((x, y) => y.logical - x.logical);
    const years = [...new Set([...monthMap.keys()].map((k) => Number(k.split("-")[0])))].sort();
    const firstYear = years.length ? Math.max(years[0], 2021) : 2024;
    const lastYear = years.length ? Math.min(years[years.length - 1], 2026) : 2026;
    const bytes: number[] = [];
    let max = 0;
    let busiest: [number, number] | null = null;
    for (let y = firstYear; y <= lastYear; y++) {
      for (let m = 0; m < 12; m++) {
        const v = monthMap.get(`${y}-${m}`) ?? 0;
        bytes.push(v);
        if (v > max) {
          max = v;
          busiest = [y, m];
        }
      }
    }
    return {
      generation: tree.generation,
      node,
      buckets,
      bucketLabels: ["Last 7 days", "8–30 days", "1–3 months", "3–12 months", "1–2 years", "Over 2 years"],
      total: buckets.reduce((x, y) => x + y, 0),
      heatmap: { firstYear, lastYear, bytes, max, busiest },
      big: big.slice(0, 60),
    };
  },
  list_children: (a) => {
    const node = Number(a.node);
    const filter = (a.filter as string | null) ?? "";
    const st = tree.stats(node);
    const rows = tree.nodes[node].children
      .filter((c) => !filter || tree.nodes[c].name.toLowerCase().includes(filter.toLowerCase()))
      .slice(0, 200)
      .map((c) => {
        const cn = tree.nodes[c];
        return {
          id: c, name: cn.name, isDir: cn.isDir, hasChildren: cn.children.length > 0,
          share: (cn.onDisk || cn.logical) / (st.onDisk || 1), size: cn.onDisk || cn.logical,
          items: cn.children.length,
          category: cn.isDir ? "Folder" : CATEGORY_LABELS[cn.category],
          color: CATEGORY_COLORS[cn.isDir ? 8 : cn.category],
          protected: cn.protected, cloud: cn.cloud, modified: cn.modified,
        };
      });
    return rows;
  },
  get_breadcrumb: (a) => {
    // Parity with Rust compute_breadcrumb: the chain INCLUDES the scan
    // root (the mock used to stop before node 0, so dev showed one
    // fewer crumb than production).
    const chain: { id: number; name: string; size: number }[] = [];
    let cur = Number(a.node);
    const ids: number[] = [];
    while (Number.isInteger(cur) && cur >= 0 && tree.nodes[cur] !== undefined) {
      ids.unshift(cur);
      if (cur === 0) break;
      cur = tree.nodes[cur].parent;
    }
    for (const id of ids) {
      chain.push({ id, name: tree.nodes[id].name, size: tree.nodes[id].onDisk || tree.nodes[id].logical });
    }
    if (chain.length === 0) chain.push({ id: 0, name: "This PC", size: tree.nodes[0].onDisk });
    return chain;
  },
  get_names: (a) => (a.ids as number[]).map((id) => tree.nodes[id]?.name ?? "?"),
  get_layout: (a) => {
    const req = a.req as Record<string, unknown>;
    const result = buildLayout(
      tree,
      Number(req.node),
      String(req.mode),
      Number(req.width),
      Number(req.height),
      Number(req.depth),
      String(req.color),
    );
    return encodeLayout(result.meta, result.cells, tree);
  },
  preview_text: () => ({ text: "The quick brown fox jumps over the lazy dog.\n".repeat(40), truncated: false, read: 1120 }),

  // ── cleanup ───────────────────────────────────────────────────────
  // Both delete modes share one body (the mock has no real shell —
  // the Windows CI integration test owns the real IFileOperation
  // proof). Mirrors recycle::plan_commit: shortest-first, nested +
  // duplicate paths absorbed, protected refused, every absorbed row
  // re-joins `trashed` (nested) so the queue clears honestly.
  commit_cleanup: (a) => commitShellDeleteMock(a, "recycle"),
  delete_permanently: (a) => commitShellDeleteMock(a, "permanent"),

  // ── duplicates ─────────────────────────────────────────────────────────────────
  // One invoke, one result — matches the engine contract. The ~900 ms
  // window runs the SAME progress cadence as Rust (phase → files →
  // bytes every ~200 ms) with the session-5 DTO v2 (monotonic `overall`
  // + cumulative `filesDoneAll`/`bytesDoneAll`), and the app-lifetime
  // status record mirrors the Rust `AppState.dupes_status` — a page
  // switch mid-scan re-attaches via `dupes_status`. Cancellation
  // rejects with "cancelled" exactly like the engine.
  find_duplicates: (a) =>
    new Promise((resolve, reject) => {
      licenseGate();
      if (dupesRunning) {
        reject("already running");
        return;
      }
      // Rust parity (session 15 "Duplicates here"): a node scopes the
      // run — only groups whose paths are ALL inside the scope's
      // subtree survive (internal duplication, the semantic the
      // engine's subtree walk produces); a root/absent scope is the
      // whole tree. A file node rejects exactly like the Rust command.
      let scopePath: string | null = null;
      const node = a.node == null ? null : Number(a.node);
      if (node != null && node !== 0) {
        const n = tree.nodes[node];
        if (n == null) {
          reject(`unknown node ${node}`);
          return;
        }
        if (!n.isDir) {
          reject("Duplicates scans a folder — select a folder.");
          return;
        }
        scopePath = fmtPath(node);
      }
      // Normalize the scope to exactly one trailing separator, then
      // prefix-match case-insensitively (Windows semantics).
      const scopePrefix = scopePath
        ? `${scopePath.toLowerCase().replace(/[\\/]+$/, "")}\\`
        : null;
      const groups = scopePrefix
        ? DUPES.filter((g) => g.files.every((f) => f.path.toLowerCase().startsWith(scopePrefix)))
        : DUPES;
      dupesRunning = true;
      dupesResult = null;
      dupesError = null;
      dupesLastProgress = null;
      dupesScopePath = scopePath;
      const latch = dupesCancelGen;
      const started = performance.now();
      const totalFiles = 1_420;
      const totalBytes = 38.2 * GB;
      const cancelled = () => dupesCancelGen !== latch;
      // Phase weights mirror the Rust PHASE_WEIGHTS (the global bar is
      // monotonic across boundaries — no 100%→0% strobe).
      const phases: { phase: "collect" | "screen" | "verify"; frac: number }[] = [
        { phase: "collect", frac: 0.02 },
        { phase: "screen", frac: 0.33 },
        { phase: "verify", frac: 0.65 },
      ];
      const duration = 850 + Math.random() * 250;
      const emit = (phase: string, overall: number) => {
        const idx = phases.findIndex((p) => p.phase === phase);
        const start = phases.slice(0, idx).reduce((s, p) => s + p.frac, 0);
        const span = phases[idx]?.frac ?? 0;
        const frac = span > 0 ? Math.min(1, (overall - start) / span) : 0;
        const known = phase !== "collect" && phase !== "done";
        const ev = {
          phase,
          filesDone: known ? Math.round(totalFiles * frac) : 0,
          filesTotal: known ? totalFiles : 0,
          bytesDone: known ? Math.round(totalBytes * frac) : 0,
          bytesTotal: known ? totalBytes : 0,
          elapsedMs: Math.round(performance.now() - started),
          filesDoneAll: Math.round(totalFiles * overall),
          bytesDoneAll: Math.round(totalBytes * overall),
          overall: Math.min(1, Math.max(0, overall)),
        };
        dupesLastProgress = ev;
        emitMockEvent("dupes-progress", ev);
      };
      let t = 0;
      const step = () => {
        if (cancelled()) {
          if (dupesTicker !== null) window.clearInterval(dupesTicker);
          dupesTicker = null;
          dupesRunning = false;
          dupesLastProgress = null;
          reject("cancelled");
          return;
        }
        t += 200;
        const overall = Math.min(1, t / duration);
        let acc = 0;
        let current = phases[phases.length - 1];
        for (const ph of phases) {
          acc += ph.frac;
          if (overall <= acc) {
            current = ph;
            break;
          }
        }
        emit(current.phase, overall);
        if (overall >= 1 && dupesTicker !== null) {
          window.clearInterval(dupesTicker);
          dupesTicker = null;
          emit("done", 1);
          const res = {
            generation: tree.generation,
            groups,
            wastedTotal: groups.reduce((sm, g) => sm + g.wasted, 0),
            files: 3_821,
            scopePath,
          };
          dupesRunning = false;
          dupesResult = res;
          dupesError = null;
          dupesLastProgress = null;
          resolve(res);
        }
      };
      dupesTicker = window.setInterval(step, 200);
      step();
    }),
  dupes_status: () => ({
    running: dupesRunning,
    generation: tree.generation,
    progress: dupesRunning ? dupesLastProgress : null,
    result: dupesResult,
    error: dupesError,
    scopePath: dupesScopePath,
  }),
  cancel_duplicates: () => {
    dupesCancelGen += 1;
    return dupesCancelGen;
  },

  // ── applications ──────────────────────────────────────────────────
  // One invoke, one list — the Rust side caches the enumeration for the
  // app lifetime (boot-time preload warms it), so per-call latency only
  // matters on the very first load.
  list_applications: () =>
    new Promise((resolve) => {
      window.setTimeout(() => resolve(APPS), 550 + Math.random() * 200);
    }),
  uninstall_app: (a) => {
    console.info("[mock] uninstall_app", a);
    return { closedProcesses: [], exitCode: 0, remainingLeftovers: [], removedEntry: true };
  },

  // ── monitor ───────────────────────────────────────────────────────
  monitor_start: () => {
    // Session mirror of the Rust engine: every start bumps the session
    // (the newest mount owns the sampler); a stop carrying a stale
    // session is ignored. Without this, the mock's random 4-22 ms IPC
    // latency can land an unmounted tab's stop AFTER the remount's
    // start — killing the live sampler (production FIFO masks the same
    // race, the session guard makes both engines immune).
    monitorSession += 1;
    if (monitorTicker === null) {
      monitorTicker = window.setInterval(() => {
        emitMockEvent("monitor-sample", monitorSample());
      }, 2000);
      emitMockEvent("monitor-sample", monitorSample());
    }
    return monitorSession;
  },
  monitor_stop: (a) => {
    const s = (a as { session?: number } | undefined)?.session;
    if (typeof s === "number" && s !== monitorSession) return null; // stale stop
    if (monitorTicker !== null) window.clearInterval(monitorTicker);
    monitorTicker = null;
    return null;
  },

  // ── snapshots ─────────────────────────────────────────────────────
  take_snapshot: (a) => {
    licenseGate();
    const node = Number(a.node ?? 0);
    const map = new Map<string, number>();
    for (const d of tree.allDescendants(node)) {
      const n = tree.nodes[d];
      if (n.isDir && (n.onDisk || n.logical) >= 1 * MB) map.set(fmtPath(d).toLowerCase(), n.onDisk || n.logical);
    }
    const st = tree.stats(node);
    const snap = {
      id: `snap-${Date.now().toString(36)}`,
      root: fmtPath(node),
      takenAt: NOW,
      total: st.onDisk,
      folders: map.size,
      map,
    };
    snapshots.push(snap);
    return { id: snap.id };
  },
  list_snapshots: () =>
    snapshots.map((s) => ({ id: s.id, root: s.root, takenAt: s.takenAt, total: s.total, folders: s.folders })),
  delete_snapshot: (a) => {
    licenseGate();
    const i = snapshots.findIndex((s) => s.id === String(a.id));
    if (i >= 0) snapshots.splice(i, 1);
    return null;
  },
  diff_snapshots: (a) => {
    licenseGate();
    const before = snapshots.find((s) => s.id === String(a.beforeId));
    const after = snapshots.find((s) => s.id === String(a.afterId));
    if (!before || !after) throw new Error("snapshot not found");
    const keys = new Set([...before.map.keys(), ...after.map.keys()]);
    const changes = [...keys]
      .map((path) => {
        const b = before.map.get(path) ?? 0;
        const af = after.map.get(path) ?? 0;
        return { path, before: b, after: af, delta: af - b };
      })
      .filter((c) => c.delta !== 0)
      .sort((x, y) => Math.abs(y.delta) - Math.abs(x.delta))
      .slice(0, 200);
    return { sameRoot: before.root === after.root, totalBefore: before.total, totalAfter: after.total, changes };
  },

  // ── license (mirrors the Rust command surface; the mock enforces
  // the SAME hard gate the Rust layer does, so browser dev exercises
  // the real lock paths) ───────────────────────────────────────────
  license_status: () => license,
  activate_license: (a) => {
    
    const normalized = normalizeKey(String(a.key ?? a.licenseKey ?? ""));
    if (!isValidKeyShape(normalized)) {
      throw "Invalid license key. Please check it and try again.";
    }
    license = {
      posture: "pro", isPro: true, tier: "lifetime",
      customerName: "Dev Tester", customerEmail: "dev@diskgenie.local",
      licenseExpiresAt: 0, graceDaysLeft: 0,
      purchaseUrl: "https://diskgenie.app/pricing", simulated: false,
    };
    window.setTimeout(() => {
      emitMockEvent("license-changed", license);
    }, 40);
    return license;
  },
  deactivate_license: () => {
    license = {
      posture: "unlicensed", isPro: false, tier: "",
      customerName: "", customerEmail: "", licenseExpiresAt: 0,
      graceDaysLeft: 0, purchaseUrl: "https://diskgenie.app/pricing",
      simulated: false,
    };
    window.setTimeout(() => {
      emitMockEvent("license-changed", license);
    }, 40);
    return license;
  },
  validate_now: () => license,
  // CI tour hook (browser-mock mirror of the Rust ci-license-sim
  // feature): flips the simulated license state.
  license_sim_set: (a) => {
    const mode = String(a?.mode ?? a?.state ?? "unlicensed");
    license = mode === "pro" ? { ...PRO_SIM } : {
      posture: "unlicensed", isPro: false, tier: "",
      customerName: "", customerEmail: "", licenseExpiresAt: 0,
      graceDaysLeft: 0, purchaseUrl: "https://diskgenie.app/pricing",
      simulated: false,
    };
    emitMockEvent("license-changed", license);
    return license;
  },
  analytics_opt_out: () => false,
  set_analytics_opt_out: () => null,
};

function ageBucketOf(modified: number, now: number): number {
  const days = (now - modified) / 86400;
  if (days <= 7) return 0;
  if (days <= 30) return 1;
  if (days <= 91) return 2;
  if (days <= 365) return 3;
  if (days <= 730) return 4;
  return 5;
}

/** Install the mock backend (browser dev/test mode). */
export function installMock(): void {
  // Tour mode boots with the simulated PRO license (the CI tour needs
  // a working app; the tour's license steps flip it back for the
  // Activation Gate captures) — EXCEPT the license program: it
  // scripts the genuine UNLICENSED boot (the real license-e2e build
  // has no sim), so the dialog auto-shows and every entry state
  // renders exactly like the runner.
  const params = new URLSearchParams(location.search);
  const tour = params.get("tour") === "1";
  const licenseProgram = (params.get("tourMode") ?? "").toLowerCase() === "license";
  if (tour && !licenseProgram) {
    license = { ...PRO_SIM };
  }
  setMockBackend(async (cmd, args) => {
    const handler = commands[cmd];
    if (!handler) {
      throw `mock: unknown command ${cmd}`;
    }
    // Simulate the async IPC boundary.
    await new Promise((r) => window.setTimeout(r, 4 + Math.random() * 18));
    const out = handler(args ?? {});
    if (out instanceof ArrayBuffer) return out;
    return out;
  });
  // Expose test-driving hooks (Playwright / agent-browser).
  (window as unknown as Record<string, unknown>).__dbMock = {
    tree: () => tree,
    rescan: (target = "ThisPC") => startScan(target, false),
    setLicense: (p: string) => {
      license = p === "pro"
        ? { ...PRO_SIM }
        : { ...license, posture: p, isPro: p === "grace", graceDaysLeft: p === "grace" ? 9 : 0 };
      emitMockEvent("license-changed", license);
    },
    reset: () => {
      tree = new MockTree();
      scannedKeys.clear();
    },
  };
}
