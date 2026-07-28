import test from "node:test";
import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const frontendRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repositoryRoot = resolve(frontendRoot, "..");
const buildScript = resolve(frontendRoot, "scripts/build.mjs");
const requiredAssets = ["console.html", "app.js", "styles.css"];

function exec(command, args, options = {}) {
  return new Promise((resolvePromise, reject) => {
    execFile(command, args, options, (error, stdout, stderr) => {
      if (error) {
        reject(
          new Error(
            `${command} ${args.join(" ")} failed: ${stderr || error.message}`
          )
        );
      } else {
        resolvePromise({ stdout, stderr });
      }
    });
  });
}

async function assertRequiredAssets(outputDirectory) {
  for (const asset of requiredAssets) {
    assert.equal((await stat(resolve(outputDirectory, asset))).isFile(), true);
  }
  const app = await readFile(resolve(outputDirectory, "app.js"), "utf8");
  assert.equal(app.includes("interface Review"), false);
  await exec(process.execPath, ["--check", resolve(outputDirectory, "app.js")]);
}

test("custom development build creates parseable fixed console assets", async () => {
  const outputDirectory = await mkdtemp(resolve(tmpdir(), "webcodex-assets-"));
  try {
    const result = await exec(process.execPath, [
      buildScript,
      "--out-dir",
      outputDirectory,
    ]);
    assert.match(result.stdout, /\[console\] built/);
    await assertRequiredAssets(outputDirectory);
  } finally {
    await rm(outputDirectory, { recursive: true, force: true });
  }
});

test(
  "watch mode performs its initial development build",
  { timeout: 15_000 },
  async () => {
    const outputDirectory = await mkdtemp(resolve(tmpdir(), "webcodex-watch-"));
    const child = spawn(
      process.execPath,
      [buildScript, "--out-dir", outputDirectory, "--watch"],
      { stdio: ["ignore", "pipe", "pipe"] }
    );
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
    });
    child.stderr.on("data", (chunk) => {
      stderr += chunk;
    });
    try {
      await new Promise((resolvePromise, reject) => {
        const deadline = setTimeout(
          () => reject(new Error(`watcher did not start: ${stdout}\n${stderr}`)),
          10_000
        );
        const inspect = () => {
          if (stdout.includes("[console] watching")) {
            clearTimeout(deadline);
            resolvePromise();
          }
        };
        child.stdout.on("data", inspect);
        child.once("exit", (code) => {
          clearTimeout(deadline);
          reject(new Error(`watcher exited early (${code}): ${stderr}`));
        });
        inspect();
      });
      assert.match(stdout, /\[console\] built/);
      await assertRequiredAssets(outputDirectory);
    } finally {
      child.kill("SIGTERM");
      await new Promise((resolvePromise) => child.once("exit", resolvePromise));
      await rm(outputDirectory, { recursive: true, force: true });
    }
  }
);

test("development output is ignored by Git", async () => {
  await exec(
    "git",
    ["check-ignore", "--quiet", "frontend/.dev-dist/app.js"],
    { cwd: repositoryRoot }
  );
});
