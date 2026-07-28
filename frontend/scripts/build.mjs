#!/usr/bin/env node
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { Script } from "node:vm";
import ts from "typescript";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const checkOnly = process.argv.includes("--check");

function read(relPath) {
  return readFileSync(resolve(root, relPath), "utf8");
}

function write(relPath, content) {
  const fullPath = resolve(root, relPath);
  mkdirSync(dirname(fullPath), { recursive: true });
  writeFileSync(fullPath, content);
}

function normalizeNewline(content) {
  return content.replace(/\r\n/g, "\n").trim() + "\n";
}

const diagnosticHost = {
  getCanonicalFileName: (fileName) => fileName,
  getCurrentDirectory: () => root,
  getNewLine: () => "\n",
};

function transpileTypeScript(relPath) {
  const result = ts.transpileModule(read(relPath), {
    compilerOptions: {
      target: ts.ScriptTarget.ES2020,
      module: ts.ModuleKind.ES2020,
      newLine: ts.NewLineKind.LineFeed,
      removeComments: false,
      sourceMap: false,
      inlineSourceMap: false,
    },
    fileName: relPath,
    reportDiagnostics: true,
  });
  const errors = (result.diagnostics || []).filter(
    (diagnostic) => diagnostic.category === ts.DiagnosticCategory.Error
  );
  if (errors.length) {
    throw new Error(ts.formatDiagnostics(errors, diagnosticHost).trim());
  }
  return normalizeNewline(result.outputText);
}

function buildJs(source) {
  // Keep generated JS readable and avoid whitespace-sensitive rewrites inside
  // template literals. TypeScript owns syntax erasure; this step only makes the
  // committed output deterministic.
  return normalizeNewline(source);
}

function assertClassicScript(relPath, source) {
  try {
    new Script(source, { filename: relPath });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new Error(`${relPath} is not valid browser JavaScript: ${message}`);
  }
}

function minifyCss(source) {
  return normalizeNewline(source)
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/\s+/g, " ")
    .replace(/\s*([{}:;,>])\s*/g, "$1")
    .replace(/;}/g, "}")
    .replace(/0\.([0-9]+)/g, ".$1")
    .trim() + "\n";
}

// Turn an ESM module into classic-script statements for inlining: drop the
// `export {}` module marker and the `export` keyword on top-level declarations.
function stripModuleExports(js) {
  return js
    .replace(/^export\s*\{\};\s*\n?/gm, "")
    .replace(/^export\s+(function|const|let|class)\b/gm, "$1");
}

// The pure review-identity state machine stays as ESM for Node tests and is also
// converted to classic-script declarations for the browser bundle.
const reviewStateModule = buildJs(transpileTypeScript("src/review_state.ts"));
const reviewStateClassic = stripModuleExports(reviewStateModule);

// app.js is one classic script: TypeScript emits valid ES2020 first, then the
// local ESM import is removed because review_state is inlined immediately above.
const appModule = transpileTypeScript("src/app.ts");
const appScript = stripModuleExports(
  appModule.replace(
    /^import\s*\{[\s\S]*?\}\s*from\s*["']\.\/review_state(?:\.js)?["'];?\s*\n/m,
    ""
  )
);
const appInlined = buildJs(reviewStateClassic + "\n" + appScript);

// Rust embeds this file as an opaque string, so validate the browser grammar
// here rather than relying on Rust compilation or TypeScript source checking.
assertClassicScript("dist/app.js", appInlined);

const outputs = new Map([
  ["dist/review_state.js", reviewStateModule],
  ["dist/app.js", appInlined],
  ["dist/styles.css", minifyCss(read("src/styles.css"))],
  // The console HTML shell is copied verbatim (no transform needed).
  ["dist/console.html", normalizeNewline(read("src/console.html"))],
]);

let drift = false;
for (const [relPath, expected] of outputs) {
  const fullPath = resolve(root, relPath);
  if (checkOnly) {
    const actual = existsSync(fullPath) ? readFileSync(fullPath, "utf8") : "";
    if (actual !== expected) {
      console.error(`${relPath} is out of date. Run: npm --prefix frontend run build`);
      drift = true;
    }
  } else {
    write(relPath, expected);
    console.log(`wrote ${relPath}`);
  }
}

if (drift) process.exit(1);
