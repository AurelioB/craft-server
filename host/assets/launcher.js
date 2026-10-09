// Renders the app lineup from status.json (published by the updater). All text is inserted with
// textContent; status values are never interpreted as HTML.

const HEARTBEAT_STALE_MS = 10 * 60 * 1000;
const COLOR = /^#[0-9a-f]{6}$/i;

function el(tag, attrs = {}, text) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
  if (text !== undefined) node.textContent = text;
  return node;
}

function hue(id) {
  let h = 0;
  for (const c of id) h = (h * 31 + c.charCodeAt(0)) % 360;
  return h;
}

function logo(app) {
  if (app.icon) return el("img", { class: "logo", src: app.icon, alt: "", width: "72", height: "72", decoding: "async" });
  return el("span", { class: "logo monogram", "aria-hidden": "true" }, app.name.slice(0, 1));
}

// "PhotoCraft" → "Photo" + accented "Craft", as on getartcraft.com.
function title(app) {
  const h = el("h2", { class: "name font-display" });
  const m = /^(.+?)(Craft)$/.exec(app.name);
  if (m) h.append(m[1], el("span", { class: "accent" }, m[2]));
  else h.textContent = app.name;
  return h;
}

function badges(app) {
  const list = el("ul", { class: "badges" });
  const add = (text, cls = "") => list.append(el("li", cls ? { class: cls } : {}, text));
  if (app.state === "ready") add(`v${app.active}`);
  else if (app.state === "installing") add("Installing", "state");
  else if (app.state === "failed") add("Install failed", "state failed");
  if (app.pinned) add("Pinned");
  if (app.pending) add(`v${app.pending} queued`);
  else if (app.update_available && app.latest) add(`v${app.latest} available`);
  return list;
}

function notes(app) {
  const out = [];
  if (app.state === "installing") out.push("Being installed; this page refreshes when it is ready.");
  if (app.state === "failed") out.push("Not available yet: the first installation failed.");
  if (app.pending) out.push(`Version ${app.pending} is downloaded and takes over after a quiet period with no visits to this app.`);
  if (app.error) out.push(`Last update problem (${app.error.stage}): ${app.error.message}`);
  return out.map((t) => el("p", { class: "note" }, t));
}

function card(app, index) {
  const ready = app.state === "ready";
  const node = ready ? el("a", { class: "app", href: app.url }) : el("div", { class: "app unavailable", "aria-disabled": "true" });
  node.style.setProperty("--app", COLOR.test(app.color) ? app.color : `hsl(${hue(app.id)} 60% 50%)`);

  const head = el("div", { class: "card-head" });
  head.append(el("span", { class: "tag" }, String(index + 1).padStart(2, "0")));
  if (app.category) head.append(el("span", { class: "hud-label" }, app.category));
  node.append(head, logo(app), title(app));
  if (app.tagline) node.append(el("p", { class: "tagline" }, app.tagline));
  node.append(badges(app), ...notes(app));
  node.append(el("span", { class: "open hud-label" }, ready ? `Open ${app.name} →` : "Not available yet"));
  return node;
}

function notices(status) {
  const box = document.getElementById("notices");
  box.replaceChildren();
  const warn = (text) => box.append(el("p", { class: "notice" }, text));
  if (!window.isSecureContext) {
    warn("This page is not served over HTTPS. Some app features (WebGPU, clipboard, browser storage) only work over HTTPS or on localhost.");
  }
  const hb = status.updater && status.updater.heartbeat_at;
  if (hb && Date.now() - Date.parse(hb) > HEARTBEAT_STALE_MS) {
    warn("The updater has not reported recently. Installed apps keep working; updates are paused.");
  }
  if (!status.apps.length) {
    warn("The updater has not published the app list yet. Apps appear here once they are installed.");
  }
}

// Who is signed in (when [auth] has a sign-in method); hidden for public, anonymous visits.
async function account() {
  let me;
  try {
    const res = await fetch("auth/me", { cache: "no-store" });
    if (!res.ok) return;
    me = await res.json();
  } catch {
    return;
  }
  if (me.method === "none") return;
  document.getElementById("account-name").textContent = `${me.user} · ${me.role}`;
  const admin = document.getElementById("account-admin");
  if (me.admin_url) {
    admin.href = me.admin_url;
    admin.hidden = false;
  }
  const logout = document.getElementById("account-logout");
  logout.hidden = !me.can_logout;
  logout.addEventListener("click", async () => {
    await fetch("auth/logout", { method: "POST" });
    location.reload();
  });
  document.getElementById("account").hidden = false;
}

async function load() {
  const grid = document.getElementById("apps");
  let status;
  try {
    const res = await fetch("status.json", { cache: "no-cache" });
    // Session ended while the page was open: reloading leads to the sign-in page.
    if (res.status === 401) return location.reload();
    status = await res.json();
  } catch {
    grid.replaceChildren(el("p", { class: "placeholder" }, "Could not load the app list. Reload to try again."));
    return;
  }
  notices(status);
  grid.replaceChildren(...status.apps.map(card));
  grid.setAttribute("aria-busy", "false");
  const ready = status.apps.filter((a) => a.state === "ready").length;
  document.getElementById("count").textContent = `${ready} of ${status.apps.length} ready`;
  document.getElementById("summary").textContent = `Self-hosted · ${status.apps.length} apps`;
  document.getElementById("updated").textContent = status.generated_at ? `Status updated ${new Date(status.generated_at).toLocaleString()}` : "";
  if (status.apps.some((a) => a.state === "installing")) setTimeout(load, 10000);
}

account();
load();
