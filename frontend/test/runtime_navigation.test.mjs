import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";
import {
  formatWorkspaceBreadcrumb,
  formatSelectedProjectIdentity,
  formatSessionWorkspaceIdentity,
  formatDeviceStatusText,
  formatProjectStatusText,
  formatRunnerCountText,
  formatRecentSessionStatusText,
} from "../dist/runtime_navigation.js";

test("operation navigation shows one destination and moves focus without replacing its form", async () => {
  const source = await readFile(new URL("../dist/runtime.js", import.meta.url), "utf8");
  const start = source.indexOf("function revealOperationsSection(");
  const end = source.indexOf("\n}", start) + 2;
  const names = ["overview", "runners", "agents"].map((name) => "runtime-operations-" + name);
  let focused;
  const panels = new Map(names.map((id) => [id, {
    hidden: id !== names[0],
    draft: "unsent message",
    focus() { focused = id; },
  }]));
  const buttons = names.map((id) => ({
    dataset: { operationsTarget: id },
    attrs: {},
    classList: { toggle() {} },
    setAttribute(key, value) { this.attrs[key] = value; },
    removeAttribute(key) { delete this.attrs[key]; },
  }));
  const scroll = { scrollTop: 800 };
  const context = vm.createContext({
    document: { querySelectorAll: () => buttons, querySelector: () => scroll },
    applyWorkspaceView() {},
    show(id, visible) { panels.get(id).hidden = !visible; },
    el: (id) => panels.get(id),
  });
  vm.runInContext(source.slice(start, end), context);
  for (const id of [names[2], names[1], names[0], names[2]]) {
    context.revealOperationsSection(id);
    assert.deepEqual([...panels].filter(([, panel]) => !panel.hidden).map(([key]) => key), [id]);
    assert.deepEqual(buttons.filter((button) => button.attrs["aria-current"] === "page").map((button) => button.dataset.operationsTarget), [id]);
    assert.equal(focused, id);
    assert.equal(scroll.scrollTop, 0);
    assert.equal(panels.get(id).draft, "unsent message");
  }
  context.revealOperationsSection("invalid");
  assert.equal(focused, names[2]);
  assert.equal(panels.get(names[2]).hidden, false);
});

test("context overview exposes validation while activity and identity have direct entries", async () => {
  const html = await readFile(new URL("../src/runtime.html", import.meta.url), "utf8");
  const overview = html.slice(html.indexOf('<section id="runtime-context-overview"'), html.indexOf('<section id="runtime-context-activity"'));
  assert.match(overview, /id="runtime-overview-validation"/);
  assert.match(overview, /id="runtime-overview-attention"/);
  assert.doesNotMatch(overview, /<details/);
  for (const name of ["overview", "activity", "details"]) {
    assert.match(html, new RegExp('data-context-target="runtime-context-' + name + '" aria-controls="runtime-context-' + name + '"'));
  }
});

test("closing mobile navigation fences its delayed focus callback", async () => {
  const source = await readFile(new URL("../dist/runtime.js", import.meta.url), "utf8");
  const start = source.indexOf("function setMobileNavigationOpen(");
  const end = source.indexOf("\n}", start) + 2;
  const classes = new Set();
  const callbacks = [];
  let focused = false;
  const shell = { classList: {
    toggle(name, enabled) { if (enabled) classes.add(name); else classes.delete(name); },
    contains(name) { return classes.has(name); },
  } };
  const context = vm.createContext({
    el(id) {
      if (id === "runtime-console") return shell;
      if (id === "runtime-mobile-nav-close") return { focus() { focused = true; } };
      return { setAttribute() {}, removeAttribute() {} };
    },
    mobileNavigationViewport: () => true,
    closeAppearanceMenus() {},
    closeTopbarMore() {},
    closeRuntimeInspector() {},
    window: { setTimeout(callback) { callbacks.push(callback); } },
  });
  vm.runInContext(source.slice(start, end), context);
  context.setMobileNavigationOpen(true);
  context.setMobileNavigationOpen(false);
  callbacks.shift()();
  assert.equal(focused, false);
  context.setMobileNavigationOpen(true);
  callbacks.shift()();
  assert.equal(focused, true);
});

test("project search opens the correct view and focuses the search on desktop and mobile", async () => {
  const source = await readFile(new URL("../dist/runtime.js", import.meta.url), "utf8");
  const start = source.indexOf("function focusProjectNavigation(");
  const end = source.indexOf("\n}", start) + 2;
  let mobile = false;
  const calls = [];
  const context = vm.createContext({
    applyWorkspaceView(view) { calls.push(view); },
    mobileNavigationViewport: () => mobile,
    setMobileNavigationOpen(...args) { calls.push(args); },
    el: (id) => ({ focus() { calls.push(id); } }),
  });
  vm.runInContext(source.slice(start, end), context);
  context.focusProjectNavigation();
  assert.deepEqual(calls.splice(0), ["sessions", "runtime-project-search"]);
  mobile = true;
  context.focusProjectNavigation();
  assert.deepEqual(calls, ["sessions", [true, false, "runtime-project-search"]]);
});

test("project shortcut respects locked state, composition, and unmodified typing", async () => {
  const source = await readFile(new URL("../dist/runtime.js", import.meta.url), "utf8");
  const start = source.indexOf('document.addEventListener("keydown", (event) => {');
  const end = source.indexOf('\n});', start) + 4;
  let handler;
  let opened = 0;
  let prevented = 0;
  const shell = { hidden: false, classList: { contains: () => false } };
  const context = vm.createContext({
    document: { addEventListener(_name, callback) { handler = callback; }, querySelector: () => null },
    el: () => shell,
    focusProjectNavigation() { opened++; },
  });
  vm.runInContext(source.slice(start, end), context);
  const key = (overrides = {}) => handler({ key: "k", preventDefault() { prevented++; }, ...overrides });
  key();
  key({ ctrlKey: true, isComposing: true });
  key({ ctrlKey: true, altKey: true });
  shell.hidden = true;
  key({ metaKey: true });
  assert.equal(opened, 0);
  assert.equal(prevented, 0);
  shell.hidden = false;
  key({ metaKey: true });
  key({ ctrlKey: true, key: "K" });
  assert.equal(opened, 2);
  assert.equal(prevented, 2);
});

test("mobile project search only receives focus while navigation remains open", async () => {
  const source = await readFile(new URL("../dist/runtime.js", import.meta.url), "utf8");
  const start = source.indexOf("function setMobileNavigationOpen(");
  const end = source.indexOf("\n}", start) + 2;
  const classes = new Set();
  const callbacks = [];
  const focused = [];
  const context = vm.createContext({
    el(id) {
      return {
        classList: {
          toggle(name, enabled) { if (enabled) classes.add(name); else classes.delete(name); },
          contains(name) { return classes.has(name); },
        },
        setAttribute() {}, removeAttribute() {}, focus() { focused.push(id); },
      };
    },
    mobileNavigationViewport: () => true,
    closeAppearanceMenus() {}, closeTopbarMore() {}, closeRuntimeInspector() {},
    window: { setTimeout(callback) { callbacks.push(callback); } },
  });
  vm.runInContext(source.slice(start, end), context);
  context.setMobileNavigationOpen(true, false, "runtime-project-search");
  context.setMobileNavigationOpen(false);
  callbacks.shift()();
  assert.deepEqual(focused, []);
  context.setMobileNavigationOpen(true, false, "runtime-project-search");
  callbacks.shift()();
  assert.deepEqual(focused, ["runtime-project-search"]);
});

test("workspace disclosure survives rerender and ignores detached toggle events", async () => {
  const source = await readFile(new URL("../dist/runtime.js", import.meta.url), "utf8");
  const start = source.indexOf('const workspace = document.createElement("details")');
  const end = source.indexOf('const row = document.createElement("summary")', start);
  const stored = new Map();
  let disclosure;
  const context = vm.createContext({
    clientId: "runner-a", project: { id: "project-a" }, state: { selectedProject: "project-a" },
    options: { selectedProject: "project-a" },
    window: { localStorage: { getItem: (key) => stored.get(key), setItem: (key, value) => stored.set(key, value) } },
    document: { createElement: () => (disclosure = {
      isConnected: true,
      addEventListener(_name, handler) { this.toggle = handler; },
    }) },
  });
  const render = () => vm.runInContext("(() => {" + source.slice(start, end) + "})()", context);
  render();
  assert.equal(disclosure.open, true);
  disclosure.open = false;
  disclosure.toggle();
  render();
  assert.equal(disclosure.open, false);
  const detached = disclosure;
  detached.isConnected = false;
  detached.open = true;
  detached.toggle();
  render();
  assert.equal(disclosure.open, false);
  context.clientId = "runner-b";
  render();
  assert.equal(disclosure.open, true);
});

test("formatWorkspaceBreadcrumb formats runner and project breadcrumb labels", () => {
  assert.deepEqual(formatWorkspaceBreadcrumb(null, "en"), {
    runnerText: "Fleet",
    projectText: "Projects",
  });
  assert.deepEqual(formatWorkspaceBreadcrumb(null, "zh-CN"), {
    runnerText: "设备群",
    projectText: "项目",
  });
  assert.deepEqual(
    formatWorkspaceBreadcrumb({ client_id: "macbook", name: "engine", id: "p1" }, "en"),
    { runnerText: "macbook", projectText: "engine" },
  );
  assert.deepEqual(
    formatWorkspaceBreadcrumb({ client_id: "macbook", id: "p1" }, "en"),
    { runnerText: "macbook", projectText: "p1" },
  );
});

test("formatSelectedProjectIdentity and formatSessionWorkspaceIdentity produce correct labels", () => {
  const project = { id: "p1", name: "Engine", path: "/opt/repo", client_id: "macbook" };
  assert.equal(
    formatSelectedProjectIdentity(project, "en"),
    "Runner: macbook · Project: p1 · Workspace: /opt/repo",
  );
  assert.equal(
    formatSelectedProjectIdentity(project, "zh-CN"),
    "运行器：macbook · 项目：p1 · 工作空间：/opt/repo",
  );
  assert.equal(
    formatSessionWorkspaceIdentity(project, "en"),
    "Runner: macbook · Project: p1 · Workspace: /opt/repo",
  );
  assert.equal(
    formatSessionWorkspaceIdentity(project, "zh-CN"),
    "运行器：macbook · 项目：p1 · 工作空间：/opt/repo",
  );
});

test("formatDeviceStatusText and formatRunnerCountText format runner counts", () => {
  assert.equal(formatDeviceStatusText(0, "", "en"), "No authorized Runners");
  assert.equal(formatDeviceStatusText(0, "", "zh-CN"), "没有已授权运行器");
  assert.equal(formatDeviceStatusText(2, "", "en"), "2 authorized Runners · All Runners");
  assert.equal(formatDeviceStatusText(2, "node-1", "en"), "2 authorized Runners · filtered");
  assert.equal(formatDeviceStatusText(2, "node-1", "zh-CN"), "2 台已授权运行器 · 已筛选");

  assert.equal(formatRunnerCountText(1, "en"), "1 Runner");
  assert.equal(formatRunnerCountText(3, "en"), "3 Runners");
  assert.equal(formatRunnerCountText(3, "zh-CN"), "3 台运行器");
});

test("formatProjectStatusText formats matching, visible, bounded, and scoped project facts", () => {
  assert.equal(
    formatProjectStatusText(5, 5, false, "", "", "en"),
    "5 visible Projects across fleet",
  );
  assert.equal(
    formatProjectStatusText(3, 3, false, "runner-1", "", "en"),
    "3 visible Projects on runner-1",
  );
  assert.equal(
    formatProjectStatusText(2, 5, true, "runner-1", "test", "en"),
    "2 of 5 matching Projects shown on runner-1 · bounded",
  );
  assert.equal(
    formatProjectStatusText(2, 5, true, "", "test", "zh-CN"),
    "已显示 2 / 5 个匹配项目 · 跨全部设备 · 有界",
  );
});

test("formatRecentSessionStatusText formats session count with optional truncation markers", () => {
  assert.equal(formatRecentSessionStatusText(null, "en"), "");
  assert.equal(
    formatRecentSessionStatusText({ returned: 4, truncated: false, scan_truncated: false }, "en"),
    "4 Sessions",
  );
  assert.equal(
    formatRecentSessionStatusText({ returned: 4, truncated: true, scan_truncated: true }, "en"),
    "4 Sessions · top 4 · partial scan",
  );
  assert.equal(
    formatRecentSessionStatusText({ returned: 4, truncated: true, scan_truncated: true }, "zh-CN"),
    "4 个会话 · 前 4 · 扫描不完整",
  );
});
