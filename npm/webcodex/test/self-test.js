"use strict";

const assert = require("assert");
const childProcess = require("child_process");
const fs = require("fs");
const http = require("http");
const os = require("os");
const path = require("path");
const { pathToFileURL } = require("url");
const zlib = require("zlib");
const install = require("../install");
const wrapper = require("../bin/wrapper");
const packageJson = require("../package.json");
const releaseManifest = require("../manifest.json");
const exampleManifest = require("../manifest.example.json");

function makeBinary(dir, name, identity = `${packageJson.version} test-revision dirty=false`) {
  const file = path.join(dir, name);
  fs.writeFileSync(file, `#!/bin/sh\nif [ "\${1-}" = "--version" ]; then echo "${name} ${identity}"; exit 0; fi\nprintf '%s\\n' "$@"\nexit "\${WEBCODEX_TEST_EXIT:-0}"\n`, { mode: 0o755 });
  return file;
}

function makeBinarySet(dir, identity) {
  fs.mkdirSync(dir, { recursive: true });
  for (const name of install.RUNTIME_BINARIES) makeBinary(dir, name, identity);
}

function archiveDirectory(sourceDir, archive) {
  const result = childProcess.spawnSync("tar", ["-czf", archive, "-C", sourceDir, "."], { encoding: "utf8" });
  assert.strictEqual(result.status, 0, result.stderr);
}

function writeTarHeader(name, size) {
  const header = Buffer.alloc(512);
  header.write(name, 0, 100, "utf8");
  header.write("0000755\0", 100, 8, "ascii");
  header.write("0000000\0", 108, 8, "ascii");
  header.write("0000000\0", 116, 8, "ascii");
  header.write(`${size.toString(8).padStart(11, "0")}\0`, 124, 12, "ascii");
  header.write("00000000000\0", 136, 12, "ascii");
  header.fill(0x20, 148, 156);
  header[156] = "0".charCodeAt(0);
  header.write("ustar\0", 257, 6, "ascii");
  let checksum = 0;
  for (const byte of header) checksum += byte;
  header.write(`${checksum.toString(8).padStart(6, "0")}\0 `, 148, 8, "ascii");
  return header;
}

function makeDeclaredEntryArchive(file, name, size) {
  const tar = Buffer.concat([writeTarHeader(name, size), Buffer.alloc(1024)]);
  fs.writeFileSync(file, zlib.gzipSync(tar));
}

async function withServer(handler, fn) {
  const server = http.createServer(handler);
  const sockets = new Set();
  server.on("connection", (socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  try {
    await fn(`http://127.0.0.1:${server.address().port}`);
  } finally {
    for (const socket of sockets) socket.destroy();
    await new Promise((resolve) => server.close(resolve));
  }
}

async function waitFor(predicate, timeoutMs = 500) {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error("Timed out waiting for test condition");
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

function manifestFor(url, sha256) {
  return {
    version: packageJson.version,
    binaries: install.RUNTIME_BINARIES,
    artifacts: { "linux-x64": { url, sha256 } }
  };
}

function writeManifest(file, manifest) {
  fs.writeFileSync(file, JSON.stringify(manifest));
}

function installedIdentity(destination) {
  return childProcess.execFileSync(path.join(destination, "webcodex"), ["--version"], { encoding: "utf8" });
}

function assertCompleteInstall(destination) {
  assert.deepStrictEqual(fs.readdirSync(destination).sort(), install.RUNTIME_BINARIES.slice().sort());
}

function assertNoInstallerTemps(tempRoot) {
  const leftovers = fs.readdirSync(tempRoot).filter((name) =>
    name.startsWith("webcodex-manifest-") || name.startsWith("webcodex-artifact-") || name.startsWith(".bin-staging-")
  );
  assert.deepStrictEqual(leftovers, []);
}

async function expectInstallFailure(action, destination, tempRoot, pattern) {
  const identityBefore = installedIdentity(destination);
  await assert.rejects(action, pattern);
  assert.strictEqual(installedIdentity(destination), identityBefore);
  assertCompleteInstall(destination);
  assertNoInstallerTemps(tempRoot);
}

async function main() {
  assert.strictEqual(packageJson.version, "0.3.0");
  assert.deepStrictEqual(packageJson.bin, { webcodex: "bin/webcodex.js" });
  assert.deepStrictEqual(install.RUNTIME_BINARIES, ["webcodex", "webcodex-server", "webcodex-runner"]);
  assert.deepStrictEqual(install.SUPPORTED_PLATFORM_KEYS, ["linux-x64", "linux-arm64", "darwin-x64", "darwin-arm64", "win32-x64"]);
  assert.strictEqual(install.MAX_MANIFEST_BYTES, 1024 * 1024);
  assert.strictEqual(install.MAX_ARTIFACT_BYTES, 128 * 1024 * 1024);
  assert.strictEqual(install.MAX_UNCOMPRESSED_BYTES, 256 * 1024 * 1024);
  assert.strictEqual(install.MAX_TAR_ENTRY_BYTES, 96 * 1024 * 1024);
  assert.strictEqual(install.MAX_REDIRECTS, 5);
  assert.strictEqual(install.resolveRedirectUrl("http://example.test/a", "https://example.test/b").protocol, "https:");
  assert.strictEqual(install.resolveRedirectUrl("http://example.test/a", "/b").protocol, "http:");
  assert.throws(() => install.resolveRedirectUrl("https://example.test/a", "http://example.test/b?token=secret"), /HTTPS downgrade/);
  assert.throws(() => install.resolveRedirectUrl("http://example.test/a", "file:\/\/\/tmp\/secret?credential=hidden"), /unsupported protocol/);
  for (const manifest of [releaseManifest, exampleManifest]) {
    assert.strictEqual(manifest.version, packageJson.version);
    assert.deepStrictEqual(manifest.binaries, install.RUNTIME_BINARIES);
    install.validateManifest(manifest);
  }

  assert.strictEqual(install.platformKey("linux", "x64"), "linux-x64");
  assert.strictEqual(install.platformKey("darwin", "arm64"), "darwin-arm64");
  assert.throws(() => install.platformKey("sunos", "x64"), /Unsupported/);
  assert.strictEqual(wrapper.nativePath({ packageRoot: "/tmp/package", platform: "linux" }), "/tmp/package/vendor/bin/webcodex");

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "webcodex-npm-test-"));
  try {
    const source = path.join(tmp, "source");
    const destination = path.join(tmp, "destination");
    makeBinarySet(source);
    const identity = install.copyLocalBinaryDir(source, { destinationDir: destination, platform: "linux" });
    assert.match(identity, /^0\.3\.0 test-revision/);
    for (const name of install.RUNTIME_BINARIES) {
      const file = path.join(destination, name);
      assert.ok(fs.statSync(file).isFile());
      assert.ok((fs.statSync(file).mode & 0o111) !== 0);
    }
    assert.ok(!fs.existsSync(path.join(destination, "webcodex-cli")));

    const oldDestination = path.join(tmp, "old-destination");
    makeBinarySet(oldDestination, `${packageJson.version} old-revision dirty=false`);
    const incomplete = path.join(tmp, "incomplete");
    fs.mkdirSync(incomplete);
    makeBinary(incomplete, "webcodex");
    makeBinary(incomplete, "webcodex-server");
    assert.throws(
      () => install.copyLocalBinaryDir(incomplete, { destinationDir: oldDestination, platform: "linux" }),
      /missing webcodex-runner/
    );
    assert.match(installedIdentity(oldDestination), /old-revision/);
    assertCompleteInstall(oldDestination);

    const mixed = path.join(tmp, "mixed");
    makeBinarySet(mixed);
    makeBinary(mixed, "webcodex-runner", `${packageJson.version} other-revision dirty=false`);
    assert.throws(
      () => install.copyLocalBinaryDir(mixed, { destinationDir: oldDestination, platform: "linux" }),
      /not from the same build/
    );
    assert.match(installedIdentity(oldDestination), /old-revision/);
    assertCompleteInstall(oldDestination);

    const archiveSource = path.join(tmp, "archive-source");
    makeBinarySet(archiveSource);
    fs.writeFileSync(path.join(archiveSource, "unrelated.txt"), "ignored");
    fs.writeFileSync(path.join(archiveSource, "webcodex-cli"), "legacy");
    const archive = path.join(tmp, "artifact.tar.gz");
    archiveDirectory(archiveSource, archive);
    const manifestPath = path.join(tmp, "manifest.json");
    writeManifest(manifestPath, manifestFor(pathToFileURL(archive).toString(), install.sha256File(archive)));
    const downloaded = path.join(tmp, "downloaded");
    await install.installFromManifest(manifestPath, { destinationDir: downloaded, platform: "linux", arch: "x64", tempDir: tmp });
    assert.deepStrictEqual(fs.readdirSync(downloaded).sort(), install.RUNTIME_BINARIES.slice().sort());

    writeManifest(manifestPath, manifestFor(pathToFileURL(archive).toString(), "0".repeat(64)));
    await expectInstallFailure(
      () => install.installFromManifest(manifestPath, { destinationDir: downloaded, platform: "linux", arch: "x64", tempDir: tmp }),
      downloaded, tmp, /checksum mismatch/
    );

    const corrupt = path.join(tmp, "corrupt.tar.gz");
    fs.writeFileSync(corrupt, "not gzip");
    writeManifest(manifestPath, manifestFor(pathToFileURL(corrupt).toString(), install.sha256File(corrupt)));
    await expectInstallFailure(
      () => install.installFromManifest(manifestPath, { destinationDir: downloaded, platform: "linux", arch: "x64", tempDir: tmp }),
      downloaded, tmp, /valid bounded gzip archive/
    );

    await withServer((_req, res) => { res.statusCode = 503; res.end("unavailable"); }, async (base) => {
      writeManifest(manifestPath, manifestFor(`${base}/artifact.tar.gz?token=secret`, install.sha256File(archive)));
      await expectInstallFailure(
        () => install.installFromManifest(manifestPath, { destinationDir: downloaded, platform: "linux", arch: "x64", tempDir: tmp }),
        downloaded, tmp, /HTTP 503/
      );
    });

    await withServer((_req, _res) => {}, async (base) => {
      await expectInstallFailure(
        () => install.installFromManifest(`${base}/manifest.json?credential=secret`, {
          destinationDir: downloaded,
          platform: "linux",
          arch: "x64",
          tempDir: tmp,
          manifestDownload: { firstByteTimeoutMs: 40, inactivityTimeoutMs: 40, totalTimeoutMs: 100 }
        }),
        downloaded, tmp, /Manifest download timed out waiting for a response/
      );
    });

    await withServer((_req, res) => {
      res.writeHead(200, { "Content-Type": "application/octet-stream" });
      res.write(Buffer.from([0x1f]));
    }, async (base) => {
      writeManifest(manifestPath, manifestFor(`${base}/artifact.tar.gz?credential=secret`, install.sha256File(archive)));
      await expectInstallFailure(
        () => install.installFromManifest(manifestPath, {
          destinationDir: downloaded,
          platform: "linux",
          arch: "x64",
          tempDir: tmp,
          artifactDownload: { firstByteTimeoutMs: 40, inactivityTimeoutMs: 40, totalTimeoutMs: 150 }
        }),
        downloaded, tmp, /Artifact download stalled before completion/
      );
    });

    await withServer((_req, res) => {
      res.writeHead(200, { "Content-Length": "4096" });
      res.end("small");
    }, async (base) => {
      writeManifest(manifestPath, manifestFor(`${base}/artifact.tar.gz`, install.sha256File(archive)));
      await expectInstallFailure(
        () => install.installFromManifest(manifestPath, {
          destinationDir: downloaded,
          platform: "linux",
          arch: "x64",
          tempDir: tmp,
          limits: { maxArtifactBytes: 1024 }
        }),
        downloaded, tmp, /1024-byte download limit/
      );
    });

    await withServer((_req, res) => {
      res.writeHead(200, { "Transfer-Encoding": "chunked" });
      res.write(Buffer.alloc(700));
      res.end(Buffer.alloc(700));
    }, async (base) => {
      writeManifest(manifestPath, manifestFor(`${base}/artifact.tar.gz`, install.sha256File(archive)));
      await expectInstallFailure(
        () => install.installFromManifest(manifestPath, {
          destinationDir: downloaded,
          platform: "linux",
          arch: "x64",
          tempDir: tmp,
          limits: { maxArtifactBytes: 1024 }
        }),
        downloaded, tmp, /1024-byte download limit/
      );
    });

    {
      const redirectSockets = new Set();
      let redirectSocket = null;
      let redirectSocketClosed = false;
      const server = http.createServer((req, res) => {
        if (req.url === "/redirect") {
          redirectSocket = req.socket;
          redirectSocket.on("close", () => { redirectSocketClosed = true; });
          res.writeHead(302, { Location: "/target" });
          res.write("redirect body never ends");
          const timer = setInterval(() => res.write("."), 10);
          res.on("close", () => clearInterval(timer));
          return;
        }
        res.end("redirect target");
      });
      server.on("connection", (socket) => {
        redirectSockets.add(socket);
        socket.on("close", () => redirectSockets.delete(socket));
      });
      await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
      const redirectDest = path.join(tmp, "redirect-target.bin");
      try {
        await install.fetchToFile(`http://127.0.0.1:${server.address().port}/redirect`, redirectDest, {
          label: "Redirect lifecycle",
          firstByteTimeoutMs: 100,
          inactivityTimeoutMs: 100,
          totalTimeoutMs: 500,
          maxBytes: 1024
        });
        assert.strictEqual(fs.readFileSync(redirectDest, "utf8"), "redirect target");
        await waitFor(() => redirectSocketClosed, 300);
        assert.ok(redirectSocket);
        assert.strictEqual(redirectSocketClosed, true);
        assert.strictEqual(redirectSocket.destroyed, true);
      } finally {
        for (const socket of redirectSockets) socket.destroy();
        await new Promise((resolve) => server.close(resolve));
        fs.rmSync(redirectDest, { force: true });
      }
    }

    await withServer((req, res) => {
      const match = /^\/deadline\/(\d+)$/.exec(req.url);
      if (!match) return res.end("done");
      const step = Number(match[1]);
      setTimeout(() => {
        res.writeHead(302, { Location: step < 3 ? `/deadline/${step + 1}` : "/target" });
        res.end();
      }, 35);
    }, async (base) => {
      const deadlineDest = path.join(tmp, "redirect-deadline.bin");
      await assert.rejects(
        () => install.fetchToFile(`${base}/deadline/0`, deadlineDest, {
          label: "Redirect deadline",
          firstByteTimeoutMs: 80,
          inactivityTimeoutMs: 80,
          totalTimeoutMs: 90,
          maxBytes: 1024
        }),
        /total timeout/
      );
      assert.ok(!fs.existsSync(deadlineDest));
    });

    await withServer((req, res) => {
      const match = /^\/count\/(\d+)$/.exec(req.url);
      const count = match ? Number(match[1]) : 0;
      if (count === 5) return res.end("five redirects succeeded");
      res.writeHead(302, { Location: `/count/${count + 1}` });
      res.end();
    }, async (base) => {
      const fiveDest = path.join(tmp, "five-redirects.bin");
      await install.fetchToFile(`${base}/count/0`, fiveDest, { label: "Redirect count", totalTimeoutMs: 500, maxBytes: 1024 });
      assert.strictEqual(fs.readFileSync(fiveDest, "utf8"), "five redirects succeeded");
      fs.rmSync(fiveDest, { force: true });
    });

    await withServer((req, res) => {
      const match = /^\/limit\/(\d+)$/.exec(req.url);
      const count = match ? Number(match[1]) : 0;
      res.writeHead(302, { Location: `/limit/${count + 1}` });
      res.end();
    }, async (base) => {
      const limitDest = path.join(tmp, "redirect-limit.bin");
      await assert.rejects(
        () => install.fetchToFile(`${base}/limit/0`, limitDest, { label: "Redirect count", totalTimeoutMs: 500, maxBytes: 1024 }),
        /redirect limit/
      );
      assert.ok(!fs.existsSync(limitDest));
    });

    await withServer((_req, res) => {
      res.writeHead(302, { Location: "file:///tmp/private?credential=hidden" });
      res.end();
    }, async (base) => {
      const protocolDest = path.join(tmp, "redirect-protocol.bin");
      await assert.rejects(
        () => install.fetchToFile(`${base}/redirect?token=secret`, protocolDest, { label: "Redirect protocol", totalTimeoutMs: 500, maxBytes: 1024 }),
        (err) => {
          assert.match(err.message, /unsupported protocol/);
          assert.doesNotMatch(err.message, /secret|hidden|credential|token/);
          return true;
        }
      );
      assert.ok(!fs.existsSync(protocolDest));
    });

    const expansion = path.join(tmp, "expansion.tar.gz");
    fs.writeFileSync(expansion, zlib.gzipSync(Buffer.alloc(4096)));
    writeManifest(manifestPath, manifestFor(pathToFileURL(expansion).toString(), install.sha256File(expansion)));
    await expectInstallFailure(
      () => install.installFromManifest(manifestPath, {
        destinationDir: downloaded,
        platform: "linux",
        arch: "x64",
        tempDir: tmp,
        limits: { maxUncompressedBytes: 1024 }
      }),
      downloaded, tmp, /1024-byte uncompressed size limit/
    );

    const oversizedEntry = path.join(tmp, "oversized-entry.tar.gz");
    makeDeclaredEntryArchive(oversizedEntry, "webcodex", 2048);
    writeManifest(manifestPath, manifestFor(pathToFileURL(oversizedEntry).toString(), install.sha256File(oversizedEntry)));
    await expectInstallFailure(
      () => install.installFromManifest(manifestPath, {
        destinationDir: downloaded,
        platform: "linux",
        arch: "x64",
        tempDir: tmp,
        limits: { maxTarEntryBytes: 1024 }
      }),
      downloaded, tmp, /1024-byte limit/
    );

    const oversizedManifest = path.join(tmp, "oversized-manifest.json");
    fs.writeFileSync(oversizedManifest, " ".repeat(2048));
    await expectInstallFailure(
      () => install.installFromManifest(oversizedManifest, {
        destinationDir: downloaded,
        platform: "linux",
        arch: "x64",
        tempDir: tmp,
        limits: { maxManifestBytes: 1024 }
      }),
      downloaded, tmp, /1024-byte size limit/
    );

    const wrapperTarget = makeBinary(tmp, "wrapper-target");
    const probe = childProcess.spawnSync(process.execPath, [path.join(__dirname, "wrapper-probe.js"), wrapperTarget, "alpha", "two words"], { encoding: "utf8" });
    assert.strictEqual(probe.status, 23, probe.stderr);
    assert.deepStrictEqual(probe.stdout.trim().split(/\r?\n/), ["alpha", "two words"]);

    const missingProbe = childProcess.spawnSync(process.execPath, [path.join(__dirname, "wrapper-missing-probe.js")], { encoding: "utf8" });
    assert.strictEqual(missingProbe.status, 127);
    assert.match(missingProbe.stderr, /installation is incomplete/);
    assert.doesNotMatch(missingProbe.stderr, /vendor\/bin/);
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
  console.log("npm wrapper, bounded download, and atomic installer self-test passed");
}

main().catch((err) => {
  console.error(err.stack || err.message);
  process.exit(1);
});
