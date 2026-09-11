import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const pnpm = process.platform === "win32" ? "pnpm.cmd" : "pnpm";
const bundleLinux = process.argv.includes("--bundle-linux");
const tauriToolsDir = resolve(root, "src-tauri", "target", ".tauri");
const gtkToolSources = [
  [resolve(root, "scripts", "linux", "linuxdeploy-plugin-gtk.sh"), "linuxdeploy-plugin-gtk.sh"],
  [resolve(root, "scripts", "linux", "linuxdeploy-plugin-gstreamer.sh"), "linuxdeploy-plugin-gstreamer.sh"],
  [resolve(root, "scripts", "linux", "bwrap"), "bwrap"],
];
const gtkRuntimeNoticeDir = resolve(root, "scripts", "linux", "gtk-runtime-notices");

async function stageGtkTools() {
  if (process.platform !== "linux") return undefined;
  mkdirSync(tauriToolsDir, { recursive: true });
  for (const [source, name] of gtkToolSources) {
    try {
      if (!statSync(source).isFile()) throw new Error("not a regular file");
    } catch {
      throw new Error(`GTK packaging tool is missing: ${source}`);
    }
    const destination = join(tauriToolsDir, name);
    copyFileSync(source, destination);
    chmodSync(destination, statSync(source).mode & 0o777);
  }
  // Keep the existing external GStreamer tool byte-for-byte, but run our
  // finalizer after its recursive linuxdeploy pass resets executable RUNPATHs.
  const upstream = join(tauriToolsDir, "cutterhoochee-gstreamer-upstream.sh");
  const checksum = "c107b49d84edbffc6ab226ed1007e0626a4f7aa2c3a36b7782bef62351d49e94";
  const digest = (bytes) => createHash("sha256").update(bytes).digest("hex");
  if (!existsSync(upstream) || digest(readFileSync(upstream)) !== checksum) {
    const response = await fetch(
      "https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gstreamer/2a2e67491c32995a3f279ad0ecbe77abd512b42a/linuxdeploy-plugin-gstreamer.sh",
      { signal: AbortSignal.timeout(30_000) },
    );
    if (!response.ok) throw new Error(`GStreamer deployment tool download failed: HTTP ${response.status}`);
    const bytes = Buffer.from(await response.arrayBuffer());
    if (digest(bytes) !== checksum) throw new Error("GStreamer deployment tool checksum mismatch");
    const temporary = `${upstream}.${process.pid}.tmp`;
    try {
      writeFileSync(temporary, bytes, { mode: 0o755 });
      renameSync(temporary, upstream);
    } finally {
      rmSync(temporary, { force: true });
    }
  }
  chmodSync(upstream, 0o755);
  return tauriToolsDir;
}

const stagedGtkToolsDir = await stageGtkTools();

if (process.platform === "linux" && !stagedGtkToolsDir) {
  throw new Error("GTK packaging tools can only be selected from the project-local Tauri tools directory");
}
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
  // The GTK plugin and wrapper are selected from target/.tauri by the
  // useLocalToolsDir setting. Runtime notices remain source-controlled.
  CUTTERHOOCHEE_GTK_RUNTIME_NOTICE_DIR: gtkRuntimeNoticeDir,
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
