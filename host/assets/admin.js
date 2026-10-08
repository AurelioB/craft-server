// Admin interface. All server values are inserted with textContent, never as HTML.

const $ = (id) => document.getElementById(id);

function el(tag, attrs = {}, text) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
  if (text !== undefined) node.textContent = text;
  return node;
}

function csrf() {
  const m = document.cookie.match(/(?:^|;\s*)craft_csrf=([^;]+)/);
  return m ? m[1] : "";
}

function when(iso) {
  return iso ? new Date(iso).toLocaleString() : "never";
}

let busy = false;

async function act(app, action, version) {
  $("error").hidden = true;
  const res = await fetch(`api/apps/${encodeURIComponent(app)}/${action}`, {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json", "X-Craft-CSRF": csrf() },
    body: JSON.stringify(version ? { version } : {}),
  });
  if (!res.ok) {
    const body = await res.json().catch(() => ({}));
    showError(body.error || `HTTP ${res.status}`);
  }
  setTimeout(load, 500);
}

function showError(message) {
  $("error").textContent = message;
  $("error").hidden = false;
}

function button(label, onClick, { primary = false, disabled = false, title } = {}) {
  const b = el("button", title ? { title } : {}, label);
  if (primary) b.className = "primary";
  b.disabled = disabled || busy;
  b.addEventListener("click", onClick);
  return b;
}

function selectWithButton(label, options, onPick) {
  const wrap = el("span", { class: "actions" });
  const select = el("select", { "aria-label": `${label}: version` });
  for (const v of options) select.append(el("option", { value: v }, v));
  wrap.append(select, button(label, () => onPick(select.value), { disabled: !options.length }));
  return wrap;
}

function row(app) {
  const tr = el("tr");
  const name = el("td");
  name.append(el("strong", {}, app.name), el("span", { class: "sub" }, `${app.id} · ${app.state}`));
  if (app.error) name.append(el("span", { class: "problem" }, `${app.error.stage}: ${app.error.message}`));
  if (app.blocked.length) name.append(el("span", { class: "sub" }, `blocked: ${app.blocked.join(", ")}`));

  const active = el("td", {}, app.active || "—");
  if (app.pinned) active.append(el("span", { class: "sub" }, `pinned ${app.pinned}`));
  active.append(el("span", { class: "sub" }, `installed: ${app.installed.join(", ") || "none"}`));

  const latest = el("td", {}, app.latest || "—");
  if (app.update_available) latest.append(el("span", { class: "sub" }, "update available"));

  const waiting = el("td", {}, app.pending || "—");
  if (app.pending) waiting.append(el("span", { class: "sub" }, "activates when idle"));

  const policy = el("td", {}, app.activation);
  policy.append(el("span", { class: "sub" }, app.auto_update ? "automatic" : "manual updates"));

  const checked = el("td", {}, when(app.last_check));
  if (app.last_check_ok === false) checked.append(el("span", { class: "problem" }, "last check failed"));

  const actions = el("td");
  const box = el("div", { class: "actions" });
  box.append(
    button("Check", () => act(app.id, "check")),
    button("Update now", () => act(app.id, "update"), { primary: app.update_available }),
  );
  if (app.pending) box.append(button(`Apply ${app.pending}`, () => act(app.id, "apply"), { primary: true }));
  const pinnable = [...new Set([app.latest, ...app.installed].filter(Boolean))];
  box.append(selectWithButton("Pin", pinnable, (v) => act(app.id, "pin", v)));
  if (app.pinned) box.append(button("Unpin", () => act(app.id, "unpin")));
  const older = app.installed.filter((v) => v !== app.active);
  if (older.length) box.append(selectWithButton("Roll back", older, (v) => act(app.id, "rollback", v)));
  for (const b of app.blocked) box.append(button(`Allow ${b}`, () => act(app.id, "allow", b)));
  actions.append(box);

  tr.append(name, active, latest, waiting, policy, checked, actions);
  return tr;
}

function logItem(text, cls) {
  const li = el("li", {}, text);
  if (cls) li.className = cls;
  return li;
}

async function load() {
  let data;
  try {
    const res = await fetch("api/status", { credentials: "same-origin", cache: "no-store" });
    if (res.status === 401) return location.reload();
    data = await res.json();
    if (!res.ok) throw new Error(data.error || `HTTP ${res.status}`);
  } catch (e) {
    showError(`Could not load status: ${e.message}`);
    setTimeout(load, 10000);
    return;
  }
  busy = Boolean(data.running);
  $("who").textContent = `Signed in as ${data.user} (${data.auth}) · updater heartbeat ${when(data.status.updater.heartbeat_at)}`;
  $("logout").hidden = !data.can_logout;
  $("running").hidden = !busy;
  $("running").textContent = busy ? `Running: ${data.running}` : "";
  document.querySelector("#apps tbody").replaceChildren(...data.status.apps.filter((a) => a.enabled).map(row));
  $("results").replaceChildren(
    ...(data.results.length
      ? data.results.map((r) => logItem(`${when(r.at)} · ${r.user}: ${r.action} ${r.app} — ${r.message}`, r.ok ? "ok" : "failed"))
      : [logItem("No actions yet.")]),
  );
  $("history").replaceChildren(
    ...data.history.map((h) =>
      logItem(`${when(h.at)} · ${h.app} ${h.action} ${h.outcome}${h.version ? " " + h.version : ""}${h.message ? " — " + h.message : ""}`, h.outcome === "failed" ? "failed" : ""),
    ),
  );
  setTimeout(load, busy ? 2000 : 30000);
}

$("logout").addEventListener("click", async () => {
  await fetch("logout", { method: "POST", credentials: "same-origin", headers: { "X-Craft-CSRF": csrf() } });
  location.href = "./";
});

load();
