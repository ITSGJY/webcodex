import { AdminHttpError, AdminRefreshController } from "./admin_controller.js";
import { AdminMutationController, AdminMutationError, MutationKind } from "./admin_mutation_controller.js";
import { renderAdminDashboard } from "./admin_view.js";

const ADMIN_BASE = "/api/admin/";
const REFRESH_MS = 10000;
const byId = (id: string) => document.getElementById(id);
const text = (id: string, value: unknown) => { const node = byId(id); if (node) node.textContent = value == null || value === "" ? "—" : String(value); };
const visible = (id: string, yes: boolean) => { const node = byId(id); if (node) node.hidden = !yes; };
let adminToken = "";
let dialogTarget = "";
let dialogContext: ReturnType<AdminMutationController["start"]> = null;
let dialogTrigger: HTMLElement | null = null;

async function parseResponse(response: Response): Promise<any> { try { return await response.json(); } catch { return null; } }
async function requestDashboard(token: string, signal: AbortSignal): Promise<unknown> {
  const response = await fetch(ADMIN_BASE + "dashboard", { method: "POST", headers: { Authorization: "Bearer " + token, "Content-Type": "application/json" }, body: "{}", signal });
  const data = await parseResponse(response);
  if (!response.ok) throw new AdminHttpError(response.status, "dashboard_failed");
  return data;
}
function classify(status: number, data: any): AdminMutationError {
  if (status === 401 || status === 403) return new AdminMutationError(status, "unauthorized");
  const code = data?.error?.code;
  const allowed = new Set(["invalid_request","revision_conflict","active_jobs_conflict","idempotency_conflict","unsupported_runner_version","agent_unavailable","operation_indeterminate","operation_failed"]);
  return new AdminMutationError(status, allowed.has(code) ? code : "operation_failed", typeof data?.active_jobs === "number" ? data.active_jobs : undefined);
}
async function requestMutation(kind: MutationKind, token: string, body: Record<string, unknown>, signal: AbortSignal): Promise<unknown> {
  const response = await fetch(`${ADMIN_BASE}projects/${kind}`, { method: "POST", headers: { Authorization: "Bearer " + token, "Content-Type": "application/json" }, body: JSON.stringify(body), signal });
  const data = await parseResponse(response); if (!response.ok) throw classify(response.status, data); return data;
}
const refresh = new AdminRefreshController<unknown>({
  request: requestDashboard,
  render: (data) => renderAdminDashboard(document, data),
  showAuthenticated: () => { visible("gate", false); visible("dashboard", true); visible("controls", true); },
  showLocked: (message) => { visible("gate", true); visible("dashboard", false); visible("controls", false); text("gate-error", message); const input = byId("token") as HTMLInputElement | null; if (input) input.value = ""; },
  setStatus: (message) => text("status", message), showError: (message) => { text("error", message); visible("error", true); }, clearError: () => visible("error", false),
});
const errorMessage: Record<string,string> = {
  invalid_request:"The request is invalid. Review the fields and try again.", revision_conflict:"Project state changed. The dashboard was refreshed; confirm again using the latest revision.", active_jobs_conflict:"The project has active jobs. No jobs were stopped; refresh and retry after they finish.", idempotency_conflict:"This retry no longer matches its original operation. Start a new operation.", unsupported_runner_version:"The Agent does not support project lifecycle operations.", agent_unavailable:"The Agent is unavailable. Current dashboard data is preserved; retry this same operation later.", operation_indeterminate:"The Agent may have completed the operation. Refresh state first, then retry this same operation context rather than creating a new mutation.", operation_failed:"The operation failed safely. Internal details were not displayed.", network_error:"Network failure. Current data is preserved; retry this same operation.", unauthorized:"Administrator authentication required."
};
const mutation = new AdminMutationController({
  request: requestMutation, keyFactory: () => crypto.randomUUID(), refresh: () => refresh.refresh(),
  outcome: (message) => { text("status", message); closeDialog(true); },
  error: (code) => { text("dialog-error", errorMessage[code]); visible("dialog-error", true); },
  pending: (_target, value) => { const submit = byId("dialog-submit") as HTMLButtonElement | null; const cancel = byId("dialog-cancel") as HTMLButtonElement | null; if (submit) { submit.disabled = value; submit.setAttribute("aria-busy", String(value)); submit.textContent = value ? "Working…" : "Continue"; } if (cancel) cancel.disabled = value; },
  lock: (message) => lock(message),
});
function lock(message = "Locked."): void { mutation.lock(); refresh.lock(message); adminToken = ""; closeDialog(false); }
function configureAutoRefresh(): void { const auto = byId("auto") as HTMLInputElement | null; if (auto?.checked) refresh.startAutoRefresh(REFRESH_MS); else refresh.stopAutoRefresh(); }
function field(name: string, label: string, type = "text", checked = false): HTMLElement { const wrap = document.createElement("label"); wrap.textContent = label; const input = document.createElement("input"); input.name = name; input.type = type; input.autocomplete = "off"; if (type === "checkbox") input.checked = checked; input.required = !["description","template"].includes(name) && type !== "checkbox"; wrap.appendChild(input); return wrap; }
function paragraph(value: string): HTMLElement { const p = document.createElement("p"); p.textContent = value; return p; }
function openCreate(kind: "register"|"create", trigger: HTMLElement): void {
  dialogTrigger = trigger; dialogTarget = `${kind}:${Date.now()}`; const title = kind === "register" ? "Register existing project" : "Create project"; text("dialog-title", title); text("dialog-description", kind === "create" ? "Create may create a directory and Git repository. Existing non-empty directories are never overwritten, and no directory deletion is offered." : "Register an existing directory. The path remains only in this form and page memory.");
  const fields = byId("dialog-fields")!; fields.replaceChildren(field("client_id","Client ID"),field("project_id","Project ID"),field("name","Name"),field("description","Description"),field("path","Path"),field("allow_patch","Allow patch","checkbox",true));
  if (kind === "create") fields.append(field("git_init","Initialize Git repository","checkbox"),field("template","Template"),field("allow_existing_empty","Allow existing empty directory","checkbox"));
  dialogContext = null; visible("dialog-error", false); (byId("project-dialog") as HTMLDialogElement).showModal(); (fields.querySelector("input") as HTMLInputElement)?.focus();
  (byId("project-form") as HTMLFormElement).dataset.kind = kind;
}
function openAction(kind: "enable"|"disable"|"unregister", project: Record<string,unknown>, trigger: HTMLElement): void {
  dialogTrigger = trigger; dialogTarget = String(project.id || ""); text("dialog-title", `${kind[0].toUpperCase()}${kind.slice(1)} project`); const fields = byId("dialog-fields")!; fields.replaceChildren();
  fields.append(paragraph(`Project: ${dialogTarget}`), paragraph(`Revision: ${String(project.revision || "")}`), paragraph(`Active jobs: ${String(project.active_jobs ?? 0)}`));
  if (kind === "disable") fields.append(paragraph("Already-started jobs will not be stopped. Project configuration and source directory are retained."));
  if (kind === "enable") fields.append(paragraph("This only re-enables a registered project. It does not create a missing directory; the Agent revalidates path policy."));
  if (kind === "unregister") { fields.append(paragraph("Only the Agent registry record is removed. The source directory and .git are not deleted. Active jobs cause rejection.")); fields.append(field("confirm_project","Type the full runtime project ID to confirm")); }
  dialogContext = mutation.start(kind, dialogTarget, { project: dialogTarget, expected_revision: String(project.revision || ""), confirm: true });
  (byId("project-form") as HTMLFormElement).dataset.kind = kind; visible("dialog-error", false); (byId("project-dialog") as HTMLDialogElement).showModal(); (fields.querySelector("input") as HTMLInputElement | null)?.focus();
}
function closeDialog(reset: boolean): void { const dialog = byId("project-dialog") as HTMLDialogElement | null; if (dialog?.open) dialog.close(); if (reset && dialogTarget) mutation.cancel(dialogTarget); const path = document.querySelector<HTMLInputElement>('input[name="path"]'); if (path) path.value = ""; dialogContext = null; dialogTarget = ""; dialogTrigger?.focus(); dialogTrigger = null; }
byId("token-form")?.addEventListener("submit", async (event) => { event.preventDefault(); const input = byId("token") as HTMLInputElement | null; const token = input?.value.trim() || ""; if (input) input.value = ""; if (!token) return; adminToken = token; mutation.beginSession(token); await refresh.beginSession(token); configureAutoRefresh(); });
byId("refresh")?.addEventListener("click", () => void refresh.refresh()); byId("lock")?.addEventListener("click", () => lock()); byId("auto")?.addEventListener("change", configureAutoRefresh);
byId("register-project")?.addEventListener("click", (e) => openCreate("register", e.currentTarget as HTMLElement)); byId("create-project")?.addEventListener("click", (e) => openCreate("create", e.currentTarget as HTMLElement));
document.addEventListener("admin-project-action", (event) => { const detail = (event as CustomEvent).detail; openAction(detail.kind, detail.project, document.querySelector(`[data-project-action="${detail.kind}"][data-project-id="${CSS.escape(String(detail.project.id))}"]`) as HTMLElement); });
byId("dialog-cancel")?.addEventListener("click", () => { if (dialogTarget) mutation.cancel(dialogTarget); closeDialog(true); });
byId("project-form")?.addEventListener("submit", (event) => { event.preventDefault(); const form = event.currentTarget as HTMLFormElement; const kind = form.dataset.kind as MutationKind; const data = new FormData(form);
  if (kind === "register" || kind === "create") { const body: Record<string,unknown> = { client_id:String(data.get("client_id")||"").trim(), project_id:String(data.get("project_id")||"").trim(), name:String(data.get("name")||"").trim(), description:String(data.get("description")||"").trim() || null, path:String(data.get("path")||"").trim(), allow_patch:data.get("allow_patch") === "on" }; if (kind === "create") Object.assign(body,{git_init:data.get("git_init") === "on",template:String(data.get("template")||"").trim() || null,allow_existing_empty:data.get("allow_existing_empty") === "on"}); dialogContext = mutation.start(kind, dialogTarget, body); }
  if (kind === "unregister" && String(data.get("confirm_project")||"") !== dialogTarget) { text("dialog-error","Type the full runtime project ID to confirm."); visible("dialog-error",true); return; }
  if (dialogContext) void mutation.submit(dialogContext);
});
window.addEventListener("pagehide", () => { mutation.dispose(); refresh.dispose(); adminToken = ""; });
refresh.lock();
