import { translate, localizedCountLabel, type RuntimeLanguage } from "./runtime_i18n.js";
import { runtimeWindowActivityLabel, runtimeWindowShortKey } from "./runtime_console_state.js";

export function windowDateTimeLabel(timestampMs: unknown, language?: RuntimeLanguage): string {
  const value = Number(timestampMs);
  if (!Number.isFinite(value) || value <= 0) return translate("time unavailable", language);
  return new Date(value).toLocaleString(language === "zh-CN" ? "zh-CN" : "en");
}

export function windowAgeLabel(timestampMs: unknown, now: number = Date.now()): string {
  return runtimeWindowActivityLabel(timestampMs, now);
}

export function runtimeProjectClientId(project: unknown): string {
  const value = String(project || "");
  const parts = value.split(":");
  return parts.length >= 3 && parts[0] === "agent" ? parts[1] : "";
}

function appendChipElement(parent: HTMLElement, text: string, extraClass = ""): HTMLElement {
  const chip = document.createElement("span");
  chip.className = "chip" + (extraClass ? " " + extraClass : "");
  chip.textContent = text;
  parent.appendChild(chip);
  return chip;
}

export function renderWindowActivityRows(
  node: HTMLElement | null,
  activities: any[],
  options: {
    compact?: boolean;
    language?: RuntimeLanguage;
    onCopyTrace?: (traceId: string) => void;
  } = {},
): void {
  if (!node) return;
  while (node.firstChild) node.removeChild(node.firstChild);
  const compact = options.compact ?? false;
  const language = options.language;
  for (const activity of activities) {
    const item = document.createElement("article");
    item.className = "window-activity-item" + (compact ? " compact" : "")
      + (activity?.recorder_gap_session_id ? " recorder-gap" : "");
    const head = document.createElement("div");
    head.className = "window-activity-head";
    const title = document.createElement("strong");
    title.textContent = String(activity?.tool_name || activity?.method || "WebCodex call");
    const time = document.createElement("span");
    time.className = "muted small";
    time.textContent = windowDateTimeLabel(activity?.started_at_ms, language);
    head.appendChild(title);
    head.appendChild(time);
    item.appendChild(head);

    const facts = document.createElement("div");
    facts.className = "chips window-activity-facts";
    appendChipElement(facts, String(activity?.status || "unknown"));
    if (activity?.project) appendChipElement(facts, String(activity.project));
    if (activity?.activity_presentation) {
      appendChipElement(facts, String(activity.activity_presentation), "tone-runtime");
    }
    if (activity?.activity_kind) appendChipElement(facts, String(activity.activity_kind));
    if (activity?.meaningful) appendChipElement(facts, "meaningful", "tone-runtime");
    if (activity?.recorder_gap_session_id) appendChipElement(facts, "recorder gap", "tone-warn");
    if (activity?.response_streaming === true) {
      appendChipElement(facts, "streaming timing unavailable", "tone-warn");
    } else if (typeof activity?.service_ms === "number") {
      appendChipElement(facts, "service " + String(activity.service_ms) + " ms");
    } else if (activity?.meaningful) {
      appendChipElement(facts, "service unavailable");
    }
    if (activity?.meaningful) {
      if (typeof activity?.next_call_gap_ms === "number") {
        appendChipElement(facts, "next gap " + String(activity.next_call_gap_ms) + " ms");
      } else {
        appendChipElement(facts, "next gap unavailable");
      }
      if (typeof activity?.cycle_ms === "number") {
        appendChipElement(facts, "cycle " + String(activity.cycle_ms) + " ms");
      }
    }
    if (activity?.window_transition_kind === "overlap") {
      appendChipElement(facts, "overlap from previous", "tone-warn");
    }
    item.appendChild(facts);

    const links = Array.isArray(activity?.workflow_sessions) ? activity.workflow_sessions : [];
    if (links.length) {
      const relation = document.createElement("div");
      relation.className = "muted small";
      relation.textContent = links
        .map((link: any) => String(link.workflow_session_id || "") + " · " + String(link.relation || "linked"))
        .join(" · ");
      item.appendChild(relation);
    }
    if (activity?.recorder_gap_session_id) {
      const gap = document.createElement("div");
      gap.className = "window-gap-note small";
      gap.textContent = "Recording was not continued for " + String(activity.recorder_gap_session_id) + ".";
      item.appendChild(gap);
    }
    if (activity?.server_trace_id) {
      const trace = document.createElement("button");
      trace.type = "button";
      trace.className = "window-trace-copy";
      trace.textContent = "trace " + String(activity.server_trace_id);
      trace.title = translate("Copy trace id", language);
      if (options.onCopyTrace) {
        trace.addEventListener("click", () => options.onCopyTrace!(String(activity.server_trace_id)));
      }
      item.appendChild(trace);
    }
    node.appendChild(item);
  }
}

export function createWindowCard(
  row: any,
  selectedWindowKey: string,
  onSelect: (key: string) => void,
  now: number = Date.now(),
  language?: RuntimeLanguage,
): HTMLElement | null {
  const key = String(row?.client_window_key || "");
  if (!key) return null;
  const button = document.createElement("button");
  button.type = "button";
  button.className = "runtime-window-card" + (key === selectedWindowKey ? " selected" : "");
  if (key === selectedWindowKey) button.setAttribute("aria-current", "true");
  const head = document.createElement("div");
  head.className = "runtime-window-card-head";
  const title = document.createElement("strong");
  title.textContent = "Window " + runtimeWindowShortKey(key);
  const active = document.createElement("span");
  active.className = "chip" + (Number(row?.active_count || 0) > 0 ? " tone-runtime" : "");
  active.textContent = Number(row?.active_count || 0) > 0
    ? (language === "zh-CN" ? String(row.active_count) + " 个活跃" : String(row.active_count) + " active")
    : String(row?.source || "window");
  head.appendChild(title);
  head.appendChild(active);
  button.appendChild(head);
  const call = document.createElement("span");
  call.className = "muted small";
  call.textContent = row?.last_tool_call_at_ms
    ? (language === "zh-CN" ? "最后调用 " : "Last WebCodex call ") + windowAgeLabel(row.last_tool_call_at_ms, now)
    : (language === "zh-CN" ? "最后活动 " : "Last WebCodex activity ") + windowAgeLabel(row?.last_seen_at_ms, now);
  button.appendChild(call);
  const meaningful = document.createElement("span");
  meaningful.className = "muted small";
  meaningful.textContent = row?.last_meaningful_activity_at_ms
    ? (language === "zh-CN" ? "最后有效工作 " : "Last meaningful work ") + windowAgeLabel(row.last_meaningful_activity_at_ms, now)
    : (language === "zh-CN" ? "未记录到有效 WebCodex 工作" : "No meaningful WebCodex work recorded");
  button.appendChild(meaningful);
  const links = document.createElement("span");
  links.className = "muted small";
  links.textContent = localizedCountLabel(Number(row?.linked_session_count || 0), "linked Session", "linked Sessions", language)
    + (Number(row?.recorder_gap_count || 0) ? " · " + String(row.recorder_gap_count) + (language === "zh-CN" ? " 个记录断层" : " recorder gap") : "");
  button.appendChild(links);
  button.addEventListener("click", () => onSelect(key));
  return button;
}

export function renderWindowActiveRequests(
  activeNode: HTMLElement | null,
  activeRequests: any[],
  options: {
    now?: number;
    onCopyTrace?: (traceId: string) => void;
  } = {},
): void {
  if (!activeNode) return;
  while (activeNode.firstChild) activeNode.removeChild(activeNode.firstChild);
  const now = options.now ?? Date.now();
  if (!activeRequests.length) {
    const empty = document.createElement("p");
    empty.className = "muted small";
    empty.textContent = "No WebCodex request is currently active.";
    activeNode.appendChild(empty);
    return;
  }
  for (const request of activeRequests) {
    const item = document.createElement("article");
    item.className = "window-request-item";
    const title = document.createElement("strong");
    title.textContent = String(request?.tool_name || request?.method || "WebCodex request");
    item.appendChild(title);
    const meta = document.createElement("div");
    meta.className = "muted small";
    const facts = [
      request?.project,
      request?.started_at_ms ? "started " + windowAgeLabel(request.started_at_ms, now) : null,
      typeof request?.elapsed_ms === "number" ? String(request.elapsed_ms) + " ms elapsed" : null,
    ].filter(Boolean).map(String);
    meta.textContent = facts.join(" · ");
    item.appendChild(meta);
    if (request?.server_trace_id) {
      const trace = document.createElement("button");
      trace.type = "button";
      trace.className = "window-trace-copy";
      trace.textContent = "trace " + String(request.server_trace_id);
      if (options.onCopyTrace) {
        trace.addEventListener("click", () => options.onCopyTrace!(String(request.server_trace_id)));
      }
      item.appendChild(trace);
    }
    activeNode.appendChild(item);
  }
}

export function renderWindowLinkedSessions(
  sessionsNode: HTMLElement | null,
  linkedSessions: any[],
  onOpenSession: (session: any) => void,
): void {
  if (!sessionsNode) return;
  while (sessionsNode.firstChild) sessionsNode.removeChild(sessionsNode.firstChild);
  for (const session of linkedSessions) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "window-session-card";
    const title = document.createElement("strong");
    title.textContent = String(session?.title || session?.workflow_session_id || "Workflow Session");
    const meta = document.createElement("span");
    meta.className = "muted small";
    meta.textContent = [
      session?.workflow_session_id,
      session?.lifecycle,
      session?.project,
      ...(Array.isArray(session?.relations) ? session.relations : []),
    ].filter(Boolean).map(String).join(" · ");
    button.appendChild(title);
    button.appendChild(meta);
    button.addEventListener("click", () => onOpenSession(session));
    sessionsNode.appendChild(button);
  }
  if (!sessionsNode.childElementCount) {
    const empty = document.createElement("p");
    empty.className = "muted small";
    empty.textContent = "No authorized Workflow Session links.";
    sessionsNode.appendChild(empty);
  }
}

export function renderSessionWindowCorrelationLinks(
  linkedNode: HTMLElement | null,
  links: any[],
  onSelectWindow: (key: string) => void,
  now: number = Date.now(),
): void {
  if (!linkedNode) return;
  while (linkedNode.firstChild) linkedNode.removeChild(linkedNode.firstChild);
  for (const link of links) {
    const key = String(link?.client_window_key || "");
    if (!key) continue;
    const button = document.createElement("button");
    button.type = "button";
    button.className = "window-session-card" + (Number(link?.recorder_gap_count || 0) ? " recorder-gap" : "");
    const title = document.createElement("strong");
    title.textContent = "Window " + runtimeWindowShortKey(key);
    const meta = document.createElement("span");
    meta.className = "muted small";
    meta.textContent = [
      link?.source,
      link?.last_seen_at_ms ? "last WebCodex activity " + windowAgeLabel(link.last_seen_at_ms, now) : null,
      Number(link?.recorder_gap_count || 0) ? String(link.recorder_gap_count) + " recorder gap" : null,
    ].filter(Boolean).map(String).join(" · ");
    button.appendChild(title);
    button.appendChild(meta);
    button.addEventListener("click", () => onSelectWindow(key));
    linkedNode.appendChild(button);
  }
  if (!links.length) {
    const empty = document.createElement("p");
    empty.className = "muted small";
    empty.textContent = "No linked Window evidence.";
    linkedNode.appendChild(empty);
  }
}

export interface RuntimeWindowDetailFields {
  title: string;
  key: string;
  source: string;
  activeCount: string;
  lastCall: string;
  lastMeaningful: string;
  activeStatus: string;
  linkedStatus: string;
  activityStatus: string;
}

export function formatWindowDetailFields(
  detail: any,
  fallbackKey = "",
  now: number = Date.now(),
  language?: RuntimeLanguage,
): RuntimeWindowDetailFields | null {
  if (!detail) return null;
  const key = String(detail.client_window_key || fallbackKey || "");
  return {
    title: "Window " + runtimeWindowShortKey(key),
    key: key || "—",
    source: String(detail.source || "—"),
    activeCount: String(Number(detail.active_count || 0)),
    lastCall: detail.last_tool_call_at_ms
      ? windowAgeLabel(detail.last_tool_call_at_ms, now)
      : "No completed tools/call activity",
    lastMeaningful: detail.last_meaningful_activity_at_ms
      ? windowAgeLabel(detail.last_meaningful_activity_at_ms, now)
      : "No meaningful WebCodex work recorded",
    activeStatus: Number(detail.active_count || 0) ? "Active request" : "No active request",
    linkedStatus:
      localizedCountLabel(Number(detail.sessions_returned || 0), "Session", "Sessions", language) +
      (detail.sessions_truncated ? " · bounded" : ""),
    activityStatus:
      localizedCountLabel(Number(detail.activity_returned || 0), "event", "events", language) +
      (detail.activity_truncated ? " · bounded" : ""),
  };
}

export function renderWindowCards(
  node: HTMLElement | null,
  windowRows: any[],
  selectedWindowKey: string,
  onSelect: (key: string) => void,
  now: number = Date.now(),
): void {
  if (!node) return;
  while (node.firstChild) node.removeChild(node.firstChild);
  for (const row of windowRows) {
    const card = createWindowCard(row, selectedWindowKey, onSelect, now);
    if (card) node.appendChild(card);
  }
}

export function renderProjectWindowCards(
  node: HTMLElement | null,
  windowRows: any[],
  onSelect: (key: string) => void,
  now: number = Date.now(),
  language?: RuntimeLanguage,
): void {
  if (!node) return;
  while (node.firstChild) node.removeChild(node.firstChild);
  for (const row of windowRows) {
    const card = createWindowCard(row, "", onSelect, now, language);
    if (card) {
      card.title = translate("Open Window inspector", language);
      node.appendChild(card);
    }
  }
}
