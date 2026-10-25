// WDU UI: everything goes through the `service` command, which forwards one
// request to the WDU service over its named pipe. Strings from catalogs are
// untrusted: they are only ever inserted with textContent.

const invoke = window.__TAURI__.core.invoke;

const SOURCES = {
  windows_update: "Windows Update",
  wu: "Windows Update",
  oem: "Catalogo del produttore",
  dell: "Dell",
  hp: "HP",
  lenovo: "Lenovo",
};
const STATES = { idle: "Inattivo", scanning: "Ricerca in corso…", installing: "Installazione in corso…" };
const REASONS = {
  newer_version: "versione più recente",
  newer_date: "driver più recente",
  no_driver: "nessun driver installato",
};
const SKIPS = {
  not_a_driver: "non è un driver (BIOS, firmware, applicazione)",
  no_hardware_ids: "il catalogo non indica i dispositivi",
  no_matching_device: "nessun dispositivo corrispondente",
  class_denied: "classe esclusa",
  class_not_allowed: "classe non consentita",
  not_newer: "già aggiornato",
  downgrade: "sarebbe un downgrade",
  unknown_version: "versione non confrontabile",
  oem_preferred: "preferito il catalogo del produttore",
  better_candidate: "c'è un aggiornamento più adatto",
};
const OUTCOMES = {
  succeeded: ["Installato", "ok"],
  failed: ["Non riuscito", "bad"],
  rolled_back: ["Non riuscito, driver precedente ripristinato", "bad"],
};

const $ = (id) => document.getElementById(id);
let lastState = null;
let selected = new Set();

async function call(request) {
  const response = await invoke("service", { request });
  if (response.type === "error") throw new Error(response.message);
  return response;
}

function el(tag, text, className) {
  const e = document.createElement(tag);
  if (text !== undefined && text !== null) e.textContent = text;
  if (className) e.className = className;
  return e;
}

function when(rfc3339) {
  if (!rfc3339) return "–";
  return new Date(rfc3339).toLocaleString("it-IT", { dateStyle: "medium", timeStyle: "short" });
}

function show(id, visible) {
  $(id).hidden = !visible;
}

// --- status -----------------------------------------------------------------

async function refreshStatus() {
  let status;
  try {
    status = (await call({ type: "status" })).status;
    show("offline", false);
  } catch (err) {
    $("offline").textContent = `Servizio WDU non raggiungibile: ${err.message ?? err}`;
    show("offline", true);
    $("scan").disabled = true;
    $("install").disabled = true;
    return;
  }

  const busy = status.state !== "idle";
  const badge = $("state");
  badge.textContent = STATES[status.state] ?? status.state;
  badge.className = "badge" + (busy ? " busy" : "");
  $("current").textContent = status.current ? `(${status.current})` : "";
  $("last-scan").textContent = status.last_scan ? when(status.last_scan) : "mai";
  $("next-scan").textContent = status.next_scan ? when(status.next_scan) : "disattivata";
  $("last-error").textContent = status.last_error ?? "";
  show("last-error", !!status.last_error);
  show("reboot", status.reboot_required);
  $("scan").disabled = busy;
  updateInstallButton(busy);

  $("warnings").replaceChildren(...status.warnings.map((w) => el("li", w)));

  // A scan or installation just ended: reload what it changed.
  if (lastState && lastState !== "idle" && !busy) {
    await Promise.all([loadPlan(), loadHistory()]);
    if (devices) await loadDrivers();
  }
  lastState = status.state;
}

function updateInstallButton(busy = lastState !== "idle") {
  const button = $("install");
  button.disabled = busy || selected.size === 0;
  button.textContent = selected.size ? `Installa selezionati (${selected.size})` : "Installa selezionati";
}

// --- plan -------------------------------------------------------------------

async function loadPlan() {
  let scan;
  try {
    scan = (await call({ type: "plan" })).scan;
  } catch {
    return;
  }
  const list = $("proposals");
  list.replaceChildren();
  selected = new Set([...selected].filter((id) => scan?.plan.proposals.some((p) => p.candidate.id === id)));

  if (!scan) {
    $("empty").textContent = "Nessuna ricerca eseguita.";
    show("empty", true);
    show("skipped-box", false);
    updateInstallButton();
    return;
  }

  const notes = [
    ...scan.wu_policy.findings.map((f) => `Policy: ${f}`),
    ...scan.source_errors.map((e) => `${SOURCES[e.source] ?? e.source} non disponibile: ${e.error}`),
  ];
  $("notes").replaceChildren(...notes.map((n) => el("li", n)));

  const proposals = scan.plan.proposals;
  $("empty").textContent = "Tutti i driver sono aggiornati.";
  show("empty", proposals.length === 0);

  for (const p of proposals) {
    const c = p.candidate;
    const item = el("li");
    const box = document.createElement("input");
    box.type = "checkbox";
    box.checked = selected.has(c.id);
    box.addEventListener("change", () => {
      box.checked ? selected.add(c.id) : selected.delete(c.id);
      updateInstallButton();
    });

    const head = el("div");
    head.append(el("span", c.title, "title"), " ");
    head.append(el("span", SOURCES[c.source] ?? c.source, "badge"));
    if (c.driver_version) head.append(" ", el("span", c.driver_version, "muted"));

    const devices = el("ul", null, "devices");
    for (const d of p.devices) {
      const from = d.installed_version ? `${d.installed_version} (${d.installed_date ?? "?"})` : "nessun driver";
      devices.append(el("li", `${d.name ?? d.instance_id} — attuale ${from}: ${REASONS[d.reason] ?? d.reason}`));
    }

    const label = el("label");
    label.append(box);
    item.append(label, head, devices);
    list.append(item);
  }

  const skipped = scan.plan.skipped;
  show("skipped-box", skipped.length > 0);
  $("skipped-count").textContent = skipped.length;
  const counts = {};
  for (const s of skipped) counts[s.reason] = (counts[s.reason] ?? 0) + 1;
  $("skipped-summary").replaceChildren(
    ...Object.entries(counts).map(([r, n]) => el("li", `${n} — ${SKIPS[r] ?? r}`)),
  );
  $("skipped").replaceChildren(
    ...skipped.map((s) => {
      const row = el("tr");
      let reason = SKIPS[s.reason] ?? s.reason;
      if (s.reason === "downgrade") reason += ` (${s.installed} → ${s.offered})`;
      if (s.class) reason += ` (${s.class})`;
      row.append(
        el("td", SOURCES[s.source] ?? s.source),
        el("td", s.title),
        el("td", s.device ?? "–", "detail"),
        el("td", reason),
      );
      return row;
    }),
  );
  updateInstallButton();
}

// --- installed drivers ------------------------------------------------------

let devices = null;
// Instance ID (lowercase) → last successful WDU installation for that device.
let installedByWdu = new Map();
let driversSort = { key: "name", dir: 1 };

// Inbox drivers keep their own INF name; packages added to the driver store
// (by Windows Update, a vendor setup or WDU) are published as oemNN.inf.
function origin(d) {
  const wdu = installedByWdu.get(d.instance_id.toLowerCase());
  if (wdu) return `Installato da WDU (${SOURCES[wdu.source] ?? wdu.source}, ${when(wdu.at)})`;
  const inf = d.driver?.inf_name;
  if (!inf) return "–";
  if (/^oem\d+\.inf$/i.test(inf)) return "Pacchetto aggiuntivo";
  return "Integrato in Windows";
}

const DRIVER_KEYS = {
  name: (d) => (d.name ?? d.instance_id).toLowerCase(),
  class: (d) => (d.class ?? "").toLowerCase(),
  version: (d) => (d.driver?.version ?? "").split(".").map((n) => n.padStart(6, "0")).join("."),
  date: (d) => d.driver?.date ?? "",
  provider: (d) => (d.driver?.provider ?? "").toLowerCase(),
  origin: (d) => origin(d).toLowerCase(),
};

async function loadDrivers() {
  $("drivers-empty").textContent = "Lettura dei dispositivi…";
  show("drivers-empty", devices === null);
  try {
    const [inventory, history] = await Promise.all([
      call({ type: "devices" }),
      call({ type: "history" }).catch(() => ({ records: [] })),
    ]);
    devices = inventory.devices;
    installedByWdu = new Map();
    for (const r of history.records) {
      if (r.outcome !== "succeeded") continue;
      for (const id of r.devices) installedByWdu.set(id.toLowerCase(), r);
    }
  } catch (err) {
    $("drivers-empty").textContent = `Impossibile leggere i dispositivi: ${err.message ?? err}`;
    show("drivers-empty", true);
    return;
  }
  renderDrivers();
}

function renderDrivers() {
  if (!devices) return;
  const query = $("drivers-filter").value.trim().toLowerCase();
  const hidden = $("drivers-hidden").checked;
  const none = $("drivers-none").checked;
  const rows = devices.filter((d) => {
    if (!hidden && d.status.hidden) return false;
    if (!none && !d.driver) return false;
    if (!query) return true;
    return [d.name, d.class, d.manufacturer, d.driver?.provider, d.driver?.version, d.driver?.inf_name, d.instance_id]
      .some((v) => v && v.toLowerCase().includes(query));
  });
  const key = DRIVER_KEYS[driversSort.key];
  rows.sort((a, b) => (key(a) < key(b) ? -1 : key(a) > key(b) ? 1 : 0) * driversSort.dir);

  for (const th of document.querySelectorAll("#tab-drivers th[data-sort]")) {
    th.classList.toggle("asc", th.dataset.sort === driversSort.key && driversSort.dir > 0);
    th.classList.toggle("desc", th.dataset.sort === driversSort.key && driversSort.dir < 0);
  }
  $("drivers-count").textContent = `${rows.length} di ${devices.length} dispositivi`;
  $("drivers-empty").textContent = "Nessun dispositivo corrisponde ai filtri.";
  show("drivers-empty", rows.length === 0);

  $("drivers").replaceChildren(
    ...rows.map((d) => {
      const name = el("td");
      name.append(d.name ?? d.instance_id);
      if (d.status.problem_code) name.append(" ", el("span", `problema ${d.status.problem_code}`, "badge bad"));
      if (d.manufacturer) name.append(el("span", d.manufacturer, "sub"));
      const from = el("td", origin(d));
      if (d.driver?.inf_name) from.append(el("span", d.driver.inf_name, "sub"));
      const row = el("tr");
      row.append(
        name,
        el("td", d.class ?? "–"),
        el("td", d.driver?.version ?? "–", "nowrap"),
        el("td", d.driver?.date ? new Date(d.driver.date).toLocaleDateString("it-IT") : "–", "nowrap"),
        el("td", d.driver?.provider ?? "–"),
        from,
      );
      return row;
    }),
  );
}

for (const th of document.querySelectorAll("#tab-drivers th[data-sort]")) {
  th.addEventListener("click", () => {
    const key = th.dataset.sort;
    driversSort = { key, dir: driversSort.key === key ? -driversSort.dir : 1 };
    renderDrivers();
  });
}
$("drivers-filter").addEventListener("input", renderDrivers);
$("drivers-hidden").addEventListener("change", renderDrivers);
$("drivers-none").addEventListener("change", renderDrivers);
$("drivers-refresh").addEventListener("click", loadDrivers);

// --- history ----------------------------------------------------------------

async function loadHistory() {
  let records;
  try {
    records = (await call({ type: "history" })).records;
  } catch {
    return;
  }
  show("history-empty", records.length === 0);
  $("history").replaceChildren(
    ...records.reverse().map((r) => {
      const [text, kind] = OUTCOMES[r.outcome] ?? [r.outcome, ""];
      const outcome = el("td");
      outcome.append(el("span", text, `badge ${kind}`));
      if (r.reboot_required) outcome.append(" ", el("span", "riavvio richiesto", "muted"));
      const row = el("tr");
      row.append(
        el("td", when(r.at)),
        el("td", SOURCES[r.source] ?? r.source),
        el("td", r.title),
        outcome,
        el("td", r.detail ?? "", "detail"),
      );
      return row;
    }),
  );
}

// --- settings ---------------------------------------------------------------

const classes = (text) => text.split(",").map((s) => s.trim()).filter(Boolean);

async function loadSettings() {
  let config;
  try {
    config = (await call({ type: "get_config" })).config;
  } catch {
    return;
  }
  $("interval").value = config.check_interval_hours;
  $("auto-install").checked = config.auto_install;
  $("wu").checked = config.windows_update;
  $("oem").checked = config.oem_catalog;
  $("oem-exclusive").checked = config.policy.oem_exclusive;
  $("deny").value = config.policy.deny_classes.join(", ");
  $("allow").value = config.policy.allow_classes.join(", ");
}

$("settings").addEventListener("submit", async (event) => {
  event.preventDefault();
  const config = {
    check_interval_hours: Number($("interval").value) || 0,
    auto_install: $("auto-install").checked,
    windows_update: $("wu").checked,
    oem_catalog: $("oem").checked,
    policy: {
      oem_exclusive: $("oem-exclusive").checked,
      deny_classes: classes($("deny").value),
      allow_classes: classes($("allow").value),
    },
  };
  try {
    await call({ type: "set_config", config });
    $("saved").textContent = "Salvato.";
    await refreshStatus();
  } catch (err) {
    $("saved").textContent = `Errore: ${err.message ?? err}`;
  }
});

// --- actions ----------------------------------------------------------------

$("scan").addEventListener("click", async () => {
  try {
    await call({ type: "scan" });
  } catch (err) {
    alert(err.message ?? err);
  }
  await refreshStatus();
});

$("install").addEventListener("click", async () => {
  const ids = [...selected];
  const ok = confirm(
    `Installare ${ids.length} aggiornament${ids.length === 1 ? "o" : "i"}?\n\n` +
      "Prima viene creato un punto di ripristino e salvato il driver attuale: " +
      "se un dispositivo smette di funzionare, il driver precedente viene ripristinato.",
  );
  if (!ok) return;
  try {
    await call({ type: "install", ids });
    selected.clear();
  } catch (err) {
    alert(err.message ?? err);
  }
  await refreshStatus();
});

for (const tab of document.querySelectorAll(".tab")) {
  tab.addEventListener("click", () => {
    for (const t of document.querySelectorAll(".tab")) t.classList.toggle("active", t === tab);
    for (const name of ["updates", "drivers", "history", "settings"]) show(`tab-${name}`, tab.dataset.tab === name);
    if (tab.dataset.tab === "drivers") loadDrivers();
    if (tab.dataset.tab === "history") loadHistory();
    if (tab.dataset.tab === "settings") loadSettings();
  });
}

async function start() {
  await refreshStatus();
  await Promise.all([loadPlan(), loadHistory()]);
  setInterval(refreshStatus, 2000);
}

start();
