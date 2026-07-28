// Read-only global admin dashboard. The credential lives only in this variable.
const ADMIN_BASE = "/api/admin/";
const REFRESH_MS = 10000;
let adminToken = "";
let timer = 0;
const byId = (id) => document.getElementById(id);
function text(id, value) { const node = byId(id); if (node)
    node.textContent = value == null || value === "" ? "—" : String(value); }
function visible(id, yes) { const node = byId(id); if (node)
    node.hidden = !yes; }
function clear(node) { while (node?.firstChild)
    node.removeChild(node.firstChild); }
function cell(row, value, code = false) { const td = document.createElement("td"); const child = code ? document.createElement("code") : document.createElement("span"); child.textContent = value == null || value === "" ? "—" : String(value); td.appendChild(child); row.appendChild(td); }
function stopAuto() { if (timer)
    window.clearInterval(timer); timer = 0; }
function startAuto() { stopAuto(); const auto = byId("auto"); if (auto?.checked && adminToken)
    timer = window.setInterval(refresh, REFRESH_MS); }
function lock(message = "") { adminToken = ""; stopAuto(); visible("gate", true); visible("dashboard", false); visible("controls", false); text("gate-error", message); const input = byId("token"); if (input)
    input.value = ""; }
async function api() { const response = await fetch(ADMIN_BASE + "dashboard", { method: "POST", headers: { Authorization: "Bearer " + adminToken, "Content-Type": "application/json" }, body: "{}" }); let data = null; try {
    data = await response.json();
}
catch { } if (!response.ok)
    throw new Error(data?.error?.message || data?.message || `Request failed (${response.status})`); return data; }
function card(label, value) { const box = document.createElement("article"); box.className = "card"; const l = document.createElement("span"); l.textContent = label; const v = document.createElement("strong"); v.textContent = value == null ? "—" : String(value); box.append(l, v); return box; }
function render(data) {
    const overview = byId("overview");
    clear(overview);
    const o = data.overview || {};
    [["Server", `${o.version || "—"} · ${o.build_commit || "unknown"}`], ["Authority", o.authority_mode], ["Agents", `${o.agents_online || 0} / ${o.agents_total || 0} online`], ["Projects", `${o.projects_online || 0} / ${o.projects_total || 0} online`], ["Active jobs", o.active_jobs || 0], ["Compatibility", o.version_compatibility || "unknown"]].forEach(([a, b]) => overview?.appendChild(card(String(a), b)));
    const devices = byId("devices");
    clear(devices);
    (data.devices || []).forEach((d) => { const r = document.createElement("tr"); [d.display_name, d.client_id, d.status, d.transport, d.hostname, d.last_seen, (d.capabilities || []).join(", "), d.project_count, d.active_jobs, d.compatibility].forEach((v, i) => cell(r, v, i === 1)); devices?.appendChild(r); });
    visible("devices-empty", !(data.devices || []).length);
    const projects = byId("projects");
    clear(projects);
    (data.projects || []).forEach((p) => { const r = document.createElement("tr"); [p.id, p.name, p.client_id, p.path, p.readiness, p.git_available, p.allow_patch, p.shell_profile_status, p.compatibility, p.console_hint].forEach((v, i) => cell(r, v, i === 0)); projects?.appendChild(r); });
    visible("projects-empty", !(data.projects || []).length);
    const diagnostics = byId("diagnostics");
    clear(diagnostics);
    Object.entries(data.diagnostics || {}).forEach(([k, v]) => { const dt = document.createElement("dt"); dt.textContent = k.replace(/_/g, " "); const dd = document.createElement("dd"); dd.textContent = typeof v === "object" ? JSON.stringify(v) : String(v ?? "—"); diagnostics?.append(dt, dd); });
    const activity = byId("activity");
    clear(activity);
    (data.activity || []).forEach((a) => { const li = document.createElement("li"); li.textContent = [a.created_at, a.kind, a.project_id, a.status].filter(Boolean).join(" · "); activity?.appendChild(li); });
    visible("activity-empty", !(data.activity || []).length);
    text("status", `Updated ${new Date().toLocaleTimeString()}`);
}
async function refresh() { if (!adminToken)
    return; visible("error", false); text("status", "Loading…"); try {
    render(await api());
    visible("gate", false);
    visible("dashboard", true);
    visible("controls", true);
}
catch (e) {
    const message = e instanceof Error ? e.message : String(e);
    text("error", message);
    visible("error", true);
    if (/auth|token|admin|unauthorized|forbidden/i.test(message))
        lock(message);
} }
byId("token-form")?.addEventListener("submit", async (e) => { e.preventDefault(); const input = byId("token"); adminToken = input?.value.trim() || ""; if (input)
    input.value = ""; await refresh(); startAuto(); });
byId("refresh")?.addEventListener("click", refresh);
byId("lock")?.addEventListener("click", () => lock("Locked."));
byId("auto")?.addEventListener("change", startAuto);
lock();
