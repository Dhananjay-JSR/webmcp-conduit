// Assemble the npm packages for a release.
//
//   node npm/build.mjs <version> <dir-with-extracted-binaries>
//
// Produces npm/dist/: one wrapper package plus one package per platform. The
// wrapper lists the platform packages as optionalDependencies, so npm installs
// only the one matching the machine and downloads nothing at install time.
import { mkdirSync, copyFileSync, writeFileSync, existsSync, rmSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const [version, binDir] = process.argv.slice(2);

if (!version || !binDir) {
  console.error("usage: node npm/build.mjs <version> <dir-with-binaries>");
  process.exit(1);
}

// rust target -> npm platform identity
const TARGETS = [
  { rust: "aarch64-apple-darwin",      os: "darwin", cpu: "arm64", exe: "conduit" },
  { rust: "x86_64-apple-darwin",       os: "darwin", cpu: "x64",   exe: "conduit" },
  { rust: "aarch64-unknown-linux-gnu", os: "linux",  cpu: "arm64", exe: "conduit" },
  { rust: "x86_64-unknown-linux-gnu",  os: "linux",  cpu: "x64",   exe: "conduit" },
  { rust: "x86_64-pc-windows-msvc",    os: "win32",  cpu: "x64",   exe: "conduit.exe" },
];

const REPO = "https://github.com/Dhananjay-JSR/webmcp-conduit";
const dist = join(here, "dist");
rmSync(dist, { recursive: true, force: true });
mkdirSync(dist, { recursive: true });

const optionalDependencies = {};
const built = [];

for (const t of TARGETS) {
  const name = `webmcp-conduit-${t.os}-${t.cpu}`;
  // The release workflow extracts each archive to a directory named after the
  // target, so the binary lands at <binDir>/<target>/conduit.
  const src = join(binDir, t.rust, t.exe);

  if (!existsSync(src)) {
    console.warn(`skipping ${name}: no binary at ${src}`);
    continue;
  }

  const pkgDir = join(dist, name);
  mkdirSync(pkgDir, { recursive: true });
  copyFileSync(src, join(pkgDir, t.exe));

  writeFileSync(
    join(pkgDir, "package.json"),
    JSON.stringify(
      {
        name,
        version,
        description: `conduit binary for ${t.os} ${t.cpu}`,
        license: "Apache-2.0",
        repository: { type: "git", url: `git+${REPO}.git` },
        // npm reads these to decide whether this optional dependency applies.
        os: [t.os],
        cpu: [t.cpu],
        files: [t.exe],
      },
      null,
      2
    ) + "\n"
  );

  // Pinned exactly: a wrapper must never pair with a different build.
  optionalDependencies[name] = version;
  built.push(name);
}

if (built.length === 0) {
  console.error("no binaries found; nothing to publish");
  process.exit(1);
}

const wrapperDir = join(dist, "webmcp-conduit");
mkdirSync(join(wrapperDir, "bin"), { recursive: true });
copyFileSync(join(here, "conduit", "bin", "conduit.js"), join(wrapperDir, "bin", "conduit.js"));
copyFileSync(join(here, "..", "README.md"), join(wrapperDir, "README.md"));
copyFileSync(join(here, "..", "LICENSE"), join(wrapperDir, "LICENSE"));

writeFileSync(
  join(wrapperDir, "package.json"),
  JSON.stringify(
    {
      name: "webmcp-conduit",
      version,
      description:
        "Turn any WebMCP-enabled website into a standard MCP server. No browser required.",
      license: "Apache-2.0",
      repository: { type: "git", url: `git+${REPO}.git` },
      homepage: REPO,
      keywords: ["mcp", "webmcp", "model-context-protocol", "ai-agents", "cli"],
      bin: { conduit: "bin/conduit.js" },
      files: ["bin/", "README.md", "LICENSE"],
      optionalDependencies,
      engines: { node: ">=18" },
    },
    null,
    2
  ) + "\n"
);

console.log(`built npm packages for ${version}:`);
for (const n of [...built, "webmcp-conduit"]) console.log(`  ${n}`);
