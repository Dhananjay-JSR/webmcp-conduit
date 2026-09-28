#!/usr/bin/env node
// Locate the platform binary and hand the process over to it.
//
// npm installs exactly one of the optionalDependencies — the one whose `os`
// and `cpu` match this machine — so resolving by name finds the right binary
// without downloading anything at install time.
"use strict";

const { spawnSync } = require("node:child_process");
const path = require("node:path");
const fs = require("node:fs");

const PLATFORMS = {
  "darwin arm64": "webmcp-conduit-darwin-arm64",
  "darwin x64": "webmcp-conduit-darwin-x64",
  "linux arm64": "webmcp-conduit-linux-arm64",
  "linux x64": "webmcp-conduit-linux-x64",
  "win32 x64": "webmcp-conduit-win32-x64",
};

function binaryPath() {
  const key = `${process.platform} ${process.arch}`;
  const pkg = PLATFORMS[key];

  if (!pkg) {
    throw new Error(
      `conduit has no prebuilt binary for ${key}.\n` +
        `Install from source instead:  cargo install webmcp-conduit`
    );
  }

  const exe = process.platform === "win32" ? "conduit.exe" : "conduit";

  // Resolve from several roots rather than just this file. A package manager
  // may symlink the wrapper (npm does this for local installs, pnpm always),
  // in which case walking up from __dirname leaves the project entirely.
  const roots = [__dirname, path.join(__dirname, ".."), process.cwd()];
  try {
    return path.join(
      path.dirname(require.resolve(`${pkg}/package.json`, { paths: roots })),
      exe
    );
  } catch {
    // Last resort: the sibling layout npm produces for a flat install.
    for (const root of roots) {
      const guess = path.join(root, "..", pkg, exe);
      if (fs.existsSync(guess)) return guess;
    }
    throw new Error(
      `conduit is missing its platform package (${pkg}).\n` +
        `This usually means the install ran with --no-optional or a lockfile\n` +
        `built on a different platform. Reinstall without --no-optional, or:\n` +
        `  cargo install webmcp-conduit`
    );
  }
}

let bin;
try {
  bin = binaryPath();
} catch (err) {
  process.stderr.write(`${err.message}\n`);
  process.exit(1);
}

if (!fs.existsSync(bin)) {
  process.stderr.write(`conduit binary not found at ${bin}\n`);
  process.exit(1);
}

// stdio is inherited on purpose: conduit speaks MCP over stdin and stdout,
// so anything that buffers or rewrites the stream would corrupt the protocol.
const result = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });

if (result.error) {
  process.stderr.write(`failed to run conduit: ${result.error.message}\n`);
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
