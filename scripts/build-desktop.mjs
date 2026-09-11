import { chmodSync, copyFileSync, mkdtempSync, rmSync, statSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const pnpm = process.platform === "win32" ? "pnpm.cmd" : "pnpm";
const bundleLinux = process.argv.includes("--bundle-linux");
const helperNames = ["gst-plugin-scanner", "gst-ptp-helper"];

function hasGstreamerHelpers(directory) {
  return (
    Boolean(directory) &&
    helperNames.every((name) => {
      try {
        return statSync(join(directory, name)).isFile();
      } catch {
        return false;
      }
    })
  );
}

function findGstreamerHelperDir() {
  if (process.platform !== "linux") return undefined;

  const multiarch = {
    x64: "x86_64",
    arm64: "aarch64",
    arm: "arm",
    ia32: "i386",
    riscv64: "riscv64",
  }[process.arch];
  const candidates = [
    process.env.GSTREAMER_HELPERS_DIR,
    process.env.GSTREAMER_PLUGINS_DIR,
    multiarch && `/usr/lib/${multiarch}-linux-gnu/gstreamer1.0/gstreamer-1.0`,
    "/usr/lib/gstreamer1.0/gstreamer-1.0",
    "/usr/lib/gstreamer-1.0",
    "/usr/libexec/gstreamer-1.0",
  ].filter(Boolean);

  return candidates.find(hasGstreamerHelpers);
}

const explicitGstreamerHelperDir = process.env.GSTREAMER_HELPERS_DIR || undefined;
if (process.platform === "linux" && explicitGstreamerHelperDir && !hasGstreamerHelpers(explicitGstreamerHelperDir)) {
  throw new Error(
    `GSTREAMER_HELPERS_DIR does not contain ${helperNames.join(" and ")}: ${explicitGstreamerHelperDir}`,
  );
}

const discoveredGstreamerHelperDir = findGstreamerHelperDir();
if (process.platform === "linux" && !discoveredGstreamerHelperDir) {
  throw new Error(
    "GStreamer helper binaries are missing; install gst-plugin-scanner and gst-ptp-helper before packaging the AppImage",
  );
}

function stageGstreamerHelpers(sourceDir) {
  const directory = mkdtempSync(join(tmpdir(), "cutterhoochee-gstreamer-helpers-"));
  try {
    for (const name of helperNames) {
      const source = join(sourceDir, name);
      const destination = join(directory, name);
      copyFileSync(source, destination);
      chmodSync(destination, statSync(source).mode & 0o777);
    }
    return directory;
  } catch (error) {
    rmSync(directory, { recursive: true, force: true });
    throw error;
  }
}

const stagedGstreamerHelperDir =
  process.platform === "linux" && !explicitGstreamerHelperDir
    ? stageGstreamerHelpers(discoveredGstreamerHelperDir)
    : undefined;
const gstreamerHelperDir = explicitGstreamerHelperDir ?? stagedGstreamerHelperDir;

const env = {
  ...process.env,
  // Tauri invokes beforeBuildCommand itself. This wrapper deliberately runs
  // the current agent and UI first so a release cannot package stale output.
  NODE_ENV: "development",
  npm_config_production: "false",
  PNPM_CONFIG_PRODUCTION: "false",
  GSTREAMER_INCLUDE_BAD_PLUGINS: "1",
  // linuxdeploy's bundled strip cannot read modern ELF RELR sections.
  // Preserve the optimized binaries instead of rewriting them with old binutils.
  NO_STRIP: "1",
  // Keep nested AppImages in extraction mode in containers without libfuse2.
  APPIMAGE_EXTRACT_AND_RUN: "1",
  CUTTERHOOCHEE_REQUIRE_AGENT: "1",
  // linuxdeploy's hook assumes Debian's helper directory. Pass a staging
  // directory containing only the bundled helper ELFs; runtime never uses PATH.
  ...(gstreamerHelperDir ? { GSTREAMER_HELPERS_DIR: gstreamerHelperDir } : {}),
};

function run(args) {
  const result = spawnSync(pnpm, args, {
    cwd: root,
    env,
    stdio: "inherit",
    shell: false,
  });
  if (result.error) throw new Error(`pnpm could not be started: ${result.error.message}`);
  if (result.status !== 0) throw new Error(`pnpm ${args.join(" ")} exited with ${result.status ?? "a signal"}`);
}

try {
  run(["build:agent"]);
  run(["build:web"]);
  run(["prepare:sidecars"]);
  run(["exec", "tauri", "build", ...(bundleLinux ? ["--bundles", "appimage", "--verbose"] : [])]);
} finally {
  if (stagedGstreamerHelperDir) {
    rmSync(stagedGstreamerHelperDir, { recursive: true, force: true });
  }
}
