import { AdminHttpError, AdminRefreshController } from "./admin_controller.js";
import { renderAdminDashboard } from "./admin_view.js";

const ADMIN_BASE = "/api/admin/";
const REFRESH_MS = 10000;

const byId = (id: string) => document.getElementById(id);

function text(id: string, value: unknown): void {
  const node = byId(id);
  if (node) node.textContent = value == null || value === "" ? "—" : String(value);
}

function visible(id: string, yes: boolean): void {
  const node = byId(id);
  if (node) node.hidden = !yes;
}

async function requestDashboard(token: string, signal: AbortSignal): Promise<unknown> {
  const response = await fetch(ADMIN_BASE + "dashboard", {
    method: "POST",
    headers: {
      Authorization: "Bearer " + token,
      "Content-Type": "application/json",
    },
    body: "{}",
    signal,
  });
  let data: any = null;
  try {
    data = await response.json();
  } catch {
    // The status code below remains the safe fallback.
  }
  if (!response.ok) {
    throw new AdminHttpError(
      response.status,
      data?.error?.message || data?.message || `Request failed (${response.status})`
    );
  }
  return data;
}

const controller = new AdminRefreshController<unknown>({
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
    const input = byId("token") as HTMLInputElement | null;
    if (input) input.value = "";
  },
  setStatus: (message) => text("status", message),
  showError: (message) => {
    text("error", message);
    visible("error", true);
  },
  clearError: () => visible("error", false),
});

function configureAutoRefresh(): void {
  const auto = byId("auto") as HTMLInputElement | null;
  if (auto?.checked) controller.startAutoRefresh(REFRESH_MS);
  else controller.stopAutoRefresh();
}

byId("token-form")?.addEventListener("submit", async (event) => {
  event.preventDefault();
  const input = byId("token") as HTMLInputElement | null;
  const token = input?.value.trim() || "";
  if (input) input.value = "";
  if (!token) return;
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
