import { renderAdminDashboard } from "./admin_view.js";

const ADMIN_BASE = "/api/admin/";
const REFRESH_MS = 10000;
let adminToken = "";
let timer = 0;

const byId = (id: string) => document.getElementById(id);

function text(id: string, value: unknown): void {
  const node = byId(id);
  if (node) node.textContent = value == null || value === "" ? "—" : String(value);
}

function visible(id: string, yes: boolean): void {
  const node = byId(id);
  if (node) node.hidden = !yes;
}

function stopAuto(): void {
  if (timer) window.clearInterval(timer);
  timer = 0;
}

function startAuto(): void {
  stopAuto();
  const auto = byId("auto") as HTMLInputElement | null;
  if (auto?.checked && adminToken) timer = window.setInterval(refresh, REFRESH_MS);
}

function lock(message = ""): void {
  adminToken = "";
  stopAuto();
  visible("gate", true);
  visible("dashboard", false);
  visible("controls", false);
  text("gate-error", message);
  const input = byId("token") as HTMLInputElement | null;
  if (input) input.value = "";
}

async function api(): Promise<unknown> {
  const response = await fetch(ADMIN_BASE + "dashboard", {
    method: "POST",
    headers: {
      Authorization: "Bearer " + adminToken,
      "Content-Type": "application/json",
    },
    body: "{}",
  });
  let data: any = null;
  try {
    data = await response.json();
  } catch {
    // The status code below remains the safe fallback.
  }
  if (!response.ok) {
    throw new Error(data?.error?.message || data?.message || `Request failed (${response.status})`);
  }
  return data;
}

async function refresh(): Promise<void> {
  if (!adminToken) return;
  visible("error", false);
  text("status", "Loading…");
  try {
    const data = await api();
    renderAdminDashboard(document, data);
    visible("gate", false);
    visible("dashboard", true);
    visible("controls", true);
    text("status", `Updated ${new Date().toLocaleTimeString()}`);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    text("error", message);
    visible("error", true);
    text("status", "Refresh failed; showing last successful data.");
    if (/auth|token|admin|unauthorized|forbidden/i.test(message)) lock(message);
  }
}

byId("token-form")?.addEventListener("submit", async (event) => {
  event.preventDefault();
  const input = byId("token") as HTMLInputElement | null;
  adminToken = input?.value.trim() || "";
  if (input) input.value = "";
  await refresh();
  startAuto();
});
byId("refresh")?.addEventListener("click", refresh);
byId("lock")?.addEventListener("click", () => lock("Locked."));
byId("auto")?.addEventListener("change", startAuto);
lock();
