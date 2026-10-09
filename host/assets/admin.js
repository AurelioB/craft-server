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
  const older = app.installed.filter((v) => v !== app.active && v !== app.pending);
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
  await fetch("../auth/logout", { method: "POST", credentials: "same-origin" });
  location.href = "./";
});

// ---- Users ----------------------------------------------------------------------------------

let users = null;
let editing = null;

// Account actions do not wait for app updates, unlike `button`.
function userButton(label, onClick, title) {
  const b = el("button", title ? { title } : {}, label);
  b.addEventListener("click", onClick);
  return b;
}
async function post(path, body) {
  const res = await fetch(path, {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json", "X-Craft-CSRF": csrf() },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data = await res.json().catch(() => ({}));
  if (res.status === 401) location.reload();
  return { ok: res.ok, data };
}

function signInText(u) {
  const ways = [];
  if (u.has_password && users.local) ways.push("password");
  if (u.oidc) ways.push(users.oidc ? `${users.oidc} (linked)` : "OIDC (linked)");
  if (!ways.length) ways.push(users.oidc ? `none yet: link ${users.oidc} or set a password` : "none: set a password");
  return ways.join(", ");
}

function userRow(u) {
  const tr = el("tr");
  const name = el("td");
  name.append(el("strong", {}, u.username));
  if (u.id === users.me) name.append(el("span", { class: "sub" }, "you"));
  tr.append(name);
  tr.append(el("td", {}, u.email || "—"));
  tr.append(el("td", {}, u.role));
  tr.append(el("td", {}, signInText(u)));
  tr.append(el("td", {}, when(u.last_login_at)));
  const actions = el("div", { class: "actions" });
  actions.append(userButton("Edit", () => openDialog(u)));
  if (u.oidc) {
    actions.append(
      userButton("Unlink", async () => {
        if (!confirm(`Remove the linked ${users.oidc || "OIDC"} identity from ${u.username}? Their sessions end.`)) return;
        const r = await post(`api/users/${u.id}/unlink`);
        if (!r.ok) showError(r.data.error || "Unlink failed");
        loadUsers();
      }, "Remove the linked sign-in identity"),
    );
  }
  if (u.id !== users.me) {
    actions.append(
      userButton("Delete", async () => {
        if (!confirm(`Delete ${u.username}? This cannot be undone.`)) return;
        const r = await post(`api/users/${u.id}/delete`);
        if (!r.ok) showError(r.data.error || "Delete failed");
        loadUsers();
      }),
    );
  }
  const cell = el("td");
  cell.append(actions);
  tr.append(cell);
  return tr;
}

async function loadUsers() {
  const res = await fetch("api/users", { credentials: "same-origin", cache: "no-store" });
  if (!res.ok) return;
  users = await res.json();
  $("users-section").hidden = !users.accounts_used;
  const notes = [];
  if (users.provider_roles) notes.push(`Roles of accounts linked to ${users.oidc} follow its groups at every sign-in.`);
  if (users.oidc) notes.push(`People can link ${users.oidc} to their account from the launcher once signed in.`);
  $("users-note").textContent = notes.join(" ");
  $("users-note").hidden = !notes.length;
  document.querySelector("#users tbody").replaceChildren(...users.users.map(userRow));
}

function openDialog(u) {
  editing = u || null;
  const form = $("user-form");
  form.reset();
  $("user-error").hidden = true;
  $("user-title").textContent = u ? `Edit ${u.username}` : "Add user";
  $("u-name").value = u ? u.username : "";
  $("u-name").disabled = Boolean(u);
  $("u-email").value = u?.email || "";
  $("u-role").value = u ? u.role : "user";
  const min = users.min_password;
  $("u-password").minLength = min;
  $("u-password").required = !u && users.local && !users.oidc;
  $("u-password-hint").textContent = u
    ? `Leave empty to keep the current password. A new one (at least ${min} characters) signs ${u.username} out everywhere.`
    : users.oidc
      ? `At least ${min} characters, or leave empty for an account that signs in with ${users.oidc} only.`
      : `At least ${min} characters.`;
  $("user-dialog").showModal();
  (u ? $("u-email") : $("u-name")).focus();
}

$("user-new").addEventListener("click", () => openDialog(null));
$("user-cancel").addEventListener("click", () => $("user-dialog").close());
$("user-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const body = { email: $("u-email").value.trim(), role: $("u-role").value };
  const password = $("u-password").value;
  if (password) body.password = password;
  const r = editing
    ? await post(`api/users/${editing.id}`, body)
    : await post("api/users", { ...body, username: $("u-name").value.trim() });
  if (!r.ok) {
    $("user-error").textContent = r.data.error || "Could not save";
    $("user-error").hidden = false;
    return;
  }
  $("user-dialog").close();
  loadUsers();
});

load();
loadUsers();
