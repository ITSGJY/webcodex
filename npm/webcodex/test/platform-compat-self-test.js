"use strict";

const assert = require("assert");
const install = require("../install");

assert.strictEqual(install.platformKey("win32", "x64"), "win32-x64");
assert.strictEqual(install.platformKey("win32", "arm64"), "win32-x64");
assert.throws(() => install.platformKey("win32", "ia32"), /Unsupported/);
assert.strictEqual(
  new Set(install.SUPPORTED_PLATFORM_KEYS).size,
  install.SUPPORTED_PLATFORM_KEYS.length,
  "artifact platform keys must remain unique when multiple host architectures share one artifact"
);
assert.deepStrictEqual(
  install.SUPPORTED_PLATFORM_KEYS,
  ["linux-x64", "linux-arm64", "darwin-x64", "darwin-arm64", "win32-x64"]
);

console.log("platform compatibility self-test passed");
