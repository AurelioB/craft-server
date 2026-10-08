// Renders the app list from status.json (published by the updater). All text is inserted with
// textContent; status values are never interpreted as HTML.

const HEARTBEAT_STALE_MS = 10 * 60 * 1000;

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

function icon(app) {
  if (app.icon) {
    const img = el("img", { class: "icon", src: app.icon, alt: "" });
    return img;
  }
  const mono = el("span", { class: "icon", "aria-hidden": "true" }, app.name.slice(0, 1));
  mono.style.background = `hsl(${hue(app.id)} 55% 45%)`;
  return mono;
}

function describe(app) {
  switch (app.state) {
    case "ready":
      return ["ready", app.pinned ? `Version ${app.active} (pinned)` : `Version ${app.active}`];
    case "installing":
      return ["installing", "Being installed; check back shortly."];
    case "failed":
      return ["failed", "Not available yet: installation failed."];
    default:
      return [app.state, ""];
  }
}

function card(app) {
  const ready = app.state === "ready";
  const node = ready ? el("a", { class: "app", href: app.url }) : el("div", { class: "app", "aria-disabled": "true" });
  const head = el("div", { class: "head" });
  head.append(icon(app));
  const title = el("div");
  title.append(el("div", { class: "name" }, app.name));
  const [cls, text] = describe(app);
  title.append(el("div", { class: ready ? "version" : `state ${cls}` }, text));
  head.append(title);
  node.append(head);
  if (app.pending) {
    node.append(el("p", { class: "note" }, `Version ${app.pending} is downloaded and will be used after a quiet period with no visits to this app.`));
  } else if (app.update_available && app.latest) {
    node.append(el("p", { class: "note" }, `Version ${app.latest} is available.`));
  }
  if (app.error) {
    node.append(el("p", { class: "note" }, `Last update problem (${app.error.stage}): ${app.error.message}`));
  }
  return node;
}

function notices(status) {
  const box = document.getElementById("notices");
  box.replaceChildren();
  if (!window.isSecureContext) {
    box.append(el("p", { class: "warn" }, "This page is not served over HTTPS. Some app features (WebGPU, clipboard, browser storage) only work over HTTPS or on localhost."));
  }
  const hb = status.updater && status.updater.heartbeat_at;
  if (hb && Date.now() - Date.parse(hb) > HEARTBEAT_STALE_MS) {
    box.append(el("p", { class: "warn" }, "The updater has not reported recently. Installed apps keep working; updates are paused."));
  }
  if (!status.apps.length) {
    box.append(el("p", {}, "The updater has not published the app list yet. Apps appear here once they are installed."));
  }
}

async function load() {
  const main = document.getElementById("apps");
  let status;
  try {
    const res = await fetch("status.json", { cache: "no-cache" });
    status = await res.json();
  } catch {
    main.replaceChildren(el("p", { class: "muted" }, "Could not load the app list. Reload to try again."));
    return;
  }
  notices(status);
  main.replaceChildren(...status.apps.map(card));
  main.setAttribute("aria-busy", "false");
  document.getElementById("updated").textContent = status.generated_at ? `Status updated ${new Date(status.generated_at).toLocaleString()}.` : "";
  if (status.apps.some((a) => a.state === "installing")) setTimeout(load, 10000);
}

load();
