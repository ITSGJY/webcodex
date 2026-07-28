class AdminHttpError extends Error {
    constructor(status, message) {
        super(message);
        this.name = "AdminHttpError";
        this.status = status;
    }
}
function isAbortError(error) {
    return error instanceof DOMException
        ? error.name === "AbortError"
        : Boolean(error && typeof error === "object" && "name" in error && error.name === "AbortError");
}
class AdminRefreshController {
    constructor(dependencies) {
        this.generation = 0;
        this.requestId = 0;
        this.token = "";
        this.active = null;
        this.timer = null;
        this.dependencies = dependencies;
    }
    beginSession(token) {
        this.invalidateRequests();
        this.token = token;
        this.dependencies.clearError();
        return this.refresh();
    }
    lock(message = "") {
        this.invalidateRequests();
        this.token = "";
        this.stopAutoRefresh();
        this.dependencies.showLocked(message);
    }
    refresh() {
        if (!this.token)
            return Promise.resolve();
        if (this.active &&
            this.active.generation === this.generation &&
            this.active.token === this.token) {
            return this.active.promise;
        }
        const generation = this.generation;
        const token = this.token;
        const id = ++this.requestId;
        const controller = new AbortController();
        this.dependencies.clearError();
        const promise = this.dependencies
            .request(token, controller.signal)
            .then((data) => {
            if (!this.isCurrent(generation, token, id))
                return;
            this.dependencies.render(data);
            this.dependencies.showAuthenticated();
            this.dependencies.setStatus(`Updated ${new Date().toLocaleTimeString()}`);
        })
            .catch((error) => {
            if (!this.isCurrent(generation, token, id) || isAbortError(error))
                return;
            if (error instanceof AdminHttpError && (error.status === 401 || error.status === 403)) {
                this.lock("Administrator authentication required.");
                return;
            }
            this.dependencies.showError("Dashboard refresh failed");
            this.dependencies.setStatus("Refresh failed; showing last successful data.");
        })
            .finally(() => {
            if (this.active?.id === id)
                this.active = null;
        });
        this.active = { generation, id, token, controller, promise };
        return promise;
    }
    startAutoRefresh(milliseconds) {
        this.stopAutoRefresh();
        if (!this.token)
            return;
        const schedule = this.dependencies.setInterval || setInterval;
        this.timer = schedule(() => {
            void this.refresh();
        }, milliseconds);
    }
    stopAutoRefresh() {
        if (this.timer === null)
            return;
        const cancel = this.dependencies.clearInterval || clearInterval;
        cancel(this.timer);
        this.timer = null;
    }
    dispose() {
        this.invalidateRequests();
        this.stopAutoRefresh();
    }
    invalidateRequests() {
        this.generation += 1;
        this.active?.controller.abort();
        this.active = null;
    }
    isCurrent(generation, token, id) {
        return (this.generation === generation &&
            this.token === token &&
            this.active?.id === id);
    }
}

function record(value) {
    return value && typeof value === "object" && !Array.isArray(value)
        ? value
        : {};
}
function list(value) {
    return Array.isArray(value) ? value : [];
}
function display(value) {
    if (value === null || value === undefined || value === "")
        return "—";
    if (typeof value === "object") {
        try {
            return JSON.stringify(value);
        }
        catch {
            return "—";
        }
    }
    return String(value);
}
function capabilityLabels(value) {
    if (Array.isArray(value)) {
        return value.filter((item) => typeof item === "string");
    }
    if (value && typeof value === "object") {
        return Object.entries(value)
            .filter(([, enabled]) => enabled === true)
            .map(([name]) => name)
            .sort();
    }
    return [];
}
function statusFor(data, section) {
    return record(record(data.section_status)[section]);
}
function sectionOk(data, section) {
    return statusFor(data, section).status !== "error";
}
function sectionError(doc, section, data) {
    const node = doc.getElementById(`${section}-error`);
    if (!node)
        return;
    const status = statusFor(data, section);
    const failed = status.status === "error";
    node.hidden = !failed;
    node.textContent = failed ? display(status.error || `${section} unavailable`) : "";
}
function clear(node) {
    while (node?.firstChild)
        node.removeChild(node.firstChild);
}
function cell(doc, row, value, code = false) {
    const td = doc.createElement("td");
    const child = doc.createElement(code ? "code" : "span");
    child.textContent = display(value);
    td.appendChild(child);
    row.appendChild(td);
}
function card(doc, label, value) {
    const box = doc.createElement("article");
    box.className = "card";
    const name = doc.createElement("span");
    name.textContent = label;
    const content = doc.createElement("strong");
    content.textContent = display(value);
    box.append(name, content);
    return box;
}
function setVisible(doc, id, visible) {
    const node = doc.getElementById(id);
    if (node)
        node.hidden = !visible;
}
function renderAdminDashboard(doc, raw) {
    const data = record(raw);
    sectionError(doc, "overview", data);
    if (sectionOk(data, "overview")) {
        const overview = doc.getElementById("overview");
        clear(overview);
        const value = record(data.overview);
        const cards = [
            ["Server", `${display(value.version)} · ${display(value.build_commit)}`],
            ["Authority", value.authority_mode],
            ["Agents", `${display(value.agents_online || 0)} / ${display(value.agents_total || 0)} online`],
            ["Projects", `${display(value.projects_online || 0)} / ${display(value.projects_total || 0)} online`],
            ["Active jobs", value.active_jobs || 0],
            ["Compatibility", value.version_compatibility || "unknown"],
        ];
        for (const [label, content] of cards)
            overview?.appendChild(card(doc, label, content));
        const diagnostics = doc.getElementById("diagnostics");
        clear(diagnostics);
        for (const [key, content] of Object.entries(record(data.diagnostics))) {
            const dt = doc.createElement("dt");
            dt.textContent = key.replace(/_/g, " ");
            const dd = doc.createElement("dd");
            dd.textContent = display(content);
            diagnostics?.append(dt, dd);
        }
    }
    sectionError(doc, "devices", data);
    if (sectionOk(data, "devices")) {
        const devices = doc.getElementById("devices");
        clear(devices);
        const rows = list(data.devices);
        for (const item of rows) {
            const device = record(item);
            const row = doc.createElement("tr");
            const values = [
                [device.display_name], [device.client_id, true], [device.status],
                [device.transport], [device.hostname], [device.last_seen],
                [capabilityLabels(device.capabilities).join(", ")], [device.project_count],
                [device.active_jobs], [device.compatibility],
            ];
            for (const [value, code] of values)
                cell(doc, row, value, Boolean(code));
            devices?.appendChild(row);
        }
        setVisible(doc, "devices-empty", rows.length === 0);
    }
    sectionError(doc, "projects", data);
    if (sectionOk(data, "projects")) {
        const projects = doc.getElementById("projects");
        clear(projects);
        const rows = list(data.projects);
        for (const item of rows) {
            const project = record(item);
            const row = doc.createElement("tr");
            const values = [
                [project.id, true], [project.name], [project.client_id], [project.path],
                [project.readiness], [project.git_available], [project.allow_patch],
                [project.shell_profile_status], [project.compatibility], [project.console_hint],
            ];
            for (const [value, code] of values)
                cell(doc, row, value, Boolean(code));
            projects?.appendChild(row);
        }
        setVisible(doc, "projects-empty", rows.length === 0);
    }
    sectionError(doc, "activity", data);
    if (sectionOk(data, "activity")) {
        const activity = doc.getElementById("activity");
        clear(activity);
        const rows = list(data.activity);
        for (const item of rows) {
            const entry = record(item);
            const li = doc.createElement("li");
            li.textContent = [entry.created_at, entry.kind, entry.project_id, entry.status]
                .filter(Boolean)
                .map(String)
                .join(" · ");
            activity?.appendChild(li);
        }
        setVisible(doc, "activity-empty", rows.length === 0);
    }
}

const ADMIN_BASE = "/api/admin/";
const REFRESH_MS = 10000;
const byId = (id) => document.getElementById(id);
function text(id, value) {
    const node = byId(id);
    if (node)
        node.textContent = value == null || value === "" ? "—" : String(value);
}
function visible(id, yes) {
    const node = byId(id);
    if (node)
        node.hidden = !yes;
}
async function requestDashboard(token, signal) {
    const response = await fetch(ADMIN_BASE + "dashboard", {
        method: "POST",
        headers: {
            Authorization: "Bearer " + token,
            "Content-Type": "application/json",
        },
        body: "{}",
        signal,
    });
    let data = null;
    try {
        data = await response.json();
    }
    catch {
        // The status code below remains the safe fallback.
    }
    if (!response.ok) {
        throw new AdminHttpError(response.status, data?.error?.message || data?.message || `Request failed (${response.status})`);
    }
    return data;
}
const controller = new AdminRefreshController({
    request: requestDashboard,
    render: (data) => renderAdminDashboard(document, data),
    showAuthenticated: () => {
        visible("gate", false);
        visible("dashboard", true);
        visible("controls", true);
    },
    showLocked: (message) => {
        visible("gate", true);
        visible("dashboard", false);
        visible("controls", false);
        text("gate-error", message);
        const input = byId("token");
        if (input)
            input.value = "";
    },
    setStatus: (message) => text("status", message),
    showError: (message) => {
        text("error", message);
        visible("error", true);
    },
    clearError: () => visible("error", false),
});
function configureAutoRefresh() {
    const auto = byId("auto");
    if (auto?.checked)
        controller.startAutoRefresh(REFRESH_MS);
    else
        controller.stopAutoRefresh();
}
byId("token-form")?.addEventListener("submit", async (event) => {
    event.preventDefault();
    const input = byId("token");
    const token = input?.value.trim() || "";
    if (input)
        input.value = "";
    if (!token)
        return;
    await controller.beginSession(token);
    configureAutoRefresh();
});
byId("refresh")?.addEventListener("click", () => {
    void controller.refresh();
});
byId("lock")?.addEventListener("click", () => controller.lock("Locked."));
byId("auto")?.addEventListener("change", configureAutoRefresh);
window.addEventListener("pagehide", () => controller.dispose());
controller.lock();
