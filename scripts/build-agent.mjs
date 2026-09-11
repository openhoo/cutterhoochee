import { access, copyFile, cp, lstat, mkdir, mkdtemp, readFile, readdir, rename, rm } from "node:fs/promises";
import { constants as fsConstants } from "node:fs";
import { basename, dirname, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const target = resolve(
  root,
  process.env.CUTTERHOOCHEE_AGENT_OUT ?? "src-tauri/resources/agent",
);
const pnpm = process.platform === "win32" ? "pnpm.cmd" : "pnpm";
const env = {
  ...process.env,
  // Keep build-time TypeScript/Vite tooling available even if the invoking
  // shell exported NODE_ENV=production. The deployed output itself contains
  // only ordinary runtime dependencies because installation is explicitly --prod.
  NODE_ENV: "development",
  npm_config_production: "false",
  PNPM_CONFIG_PRODUCTION: "false",
};

function run(args, cwd = root) {
  const result = spawnSync(pnpm, args, {
    cwd,
    env,
    stdio: "inherit",
    shell: false,
  });
  if (result.error) throw new Error(`pnpm could not be started: ${result.error.message}`);
  if (result.status !== 0) {
    throw new Error(`pnpm ${args.join(" ")} exited with ${result.status ?? "a signal"}`);
  }
}

async function requireFile(path, label) {
  try {
    await access(path, fsConstants.R_OK);
  } catch {
    throw new Error(`agent deployment is missing ${label}: ${path}`);
  }
}

async function pathExists(path) {
  try {
    await lstat(path);
    return true;
  } catch (error) {
    if (error?.code === "ENOENT") return false;
    throw error;
  }
}

async function createSibling(kind) {
  return mkdtemp(resolve(dirname(target), `.${basename(target)}.${kind}-`));
}

async function reserveSibling(kind) {
  const path = await createSibling(kind);
  await rm(path, { recursive: true, force: true });
  return path;
}

async function validateDeployment(deployment) {
  const agent = resolve(deployment, "agent");
  const shared = resolve(deployment, "shared");
  await requireFile(resolve(agent, "dist", "main.js"), "compiled entrypoint");
  await requireFile(resolve(agent, "package.json"), "deployed agent package metadata");
  await requireFile(resolve(agent, "node_modules"), "agent importer runtime dependencies");
  await requireFile(resolve(shared, "package.json"), "deployed shared package metadata");
  await requireFile(resolve(shared, "dist", "index.js"), "compiled shared package");
  await requireFile(resolve(deployment, "node_modules"), "root hoisted runtime dependencies");
  const packageJson = JSON.parse(await readFile(resolve(agent, "package.json"), "utf8"));
  if (packageJson.dependencies?.typebox !== "1.3.7") {
    throw new Error("deployed agent package does not pin typebox 1.3.7");
  }
  await validateImporterRuntime(deployment, packageJson);
  await assertNoSymlinks(deployment);
}

async function assertNoSymlinks(directory, relative = ".") {
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const entryPath = resolve(directory, entry.name);
    const entryRelative = relative === "." ? entry.name : `${relative}/${entry.name}`;
    if (entry.isSymbolicLink()) {
      throw new Error(`agent deployment still contains a symlink at ${entryRelative}`);
    }
    if (entry.isDirectory()) {
      await assertNoSymlinks(entryPath, entryRelative);
    }
  }
}

async function validateImporterRuntime(deployment, packageJson) {
  const importerNodeModules = resolve(deployment, "agent", "node_modules");
  const rootNodeModules = resolve(deployment, "node_modules");
  await requireFile(
    resolve(importerNodeModules, "@cutterhoochee", "shared"),
    "importer-local @cutterhoochee/shared",
  );
  const dependencies = new Set([
    ...Object.keys(packageJson.dependencies ?? {}),
    ...Object.keys(packageJson.optionalDependencies ?? {}),
  ]);

  for (const dependency of dependencies) {
    const importerPath = resolve(importerNodeModules, dependency);
    if (await pathExists(importerPath)) {
      await requireFile(importerPath, `agent importer runtime dependency ${dependency}`);
    } else {
      await requireFile(
        resolve(rootNodeModules, dependency),
        `root runtime dependency ${dependency}`,
      );
    }
  }
}

async function copyWorkspaceRuntime(workspace, deployment) {
  // Keep each workspace-relative lookup root distinct. The importer can own
  // @cutterhoochee/shared or a collision-selected SDK while the hoisted root
  // owns other packages. Dereference links within each root, but never merge
  // package-local and root node_modules into one deployment directory.
  for (const [relativePath, label] of [
    ["agent", "agent importer runtime"],
    ["shared", "compiled shared package"],
    ["node_modules", "root hoisted runtime"],
  ]) {
    const source = resolve(workspace, relativePath);
    await requireFile(source, label);
    await cp(source, resolve(deployment, relativePath), {
      recursive: true,
      dereference: true,
      force: true,
    });
  }
}

async function copyDeployWorkspaceFile(workspace, relativePath) {
  const source = resolve(root, relativePath);
  const destination = resolve(workspace, relativePath);
  await requireFile(source, relativePath);
  await mkdir(dirname(destination), { recursive: true });
  await copyFile(source, destination);
}

async function populateDeployWorkspace(workspace) {
  // Keep pnpm's workspace graph and lockfile unchanged, but give deploy a
  // private root so its production-only workspace state cannot touch the
  // development checkout.
  for (const relativePath of [
    "package.json",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "src/package.json",
    "shared/package.json",
    "agent/package.json",
  ]) {
    await copyDeployWorkspaceFile(workspace, relativePath);
  }

  await cp(resolve(root, "shared", "dist"), resolve(workspace, "shared", "dist"), {
    recursive: true,
  });
  await cp(resolve(root, "agent", "dist"), resolve(workspace, "agent", "dist"), {
    recursive: true,
  });
}

await mkdir(dirname(target), { recursive: true });

let staging;
let workspace;
let backup;
let targetMoved = false;
let promoted = false;
let rollbackAttempted = false;

try {
  staging = await createSibling("staging");
  run(["--filter=@cutterhoochee/shared", "build"]);
  run(["--filter=@cutterhoochee/agent", "build"]);

  workspace = await createSibling("workspace");
  await populateDeployWorkspace(workspace);
  // The filtered production install is copied as three workspace-relative
  // roots. Preserve importer-local links, shared package context, and the
  // hoisted root independently before removing the temporary workspace.
  run(
    [
      "--filter=@cutterhoochee/agent...",
      "install",
      "--prod",
      "--frozen-lockfile",
      "--node-linker=hoisted",
      "--config.enable-global-virtual-store=false",
    ],
    workspace,
  );
  await copyWorkspaceRuntime(workspace, staging);
  await validateDeployment(staging);

  if (await pathExists(target)) {
    backup = await reserveSibling("backup");
    await rename(target, backup);
    targetMoved = true;
  }

  try {
    await rename(staging, target);
    staging = undefined;
    promoted = true;
  } catch (promotionError) {
    if (targetMoved) {
      rollbackAttempted = true;
      try {
        await rename(backup, target);
        targetMoved = false;
      } catch (rollbackError) {
        throw new AggregateError(
          [promotionError, rollbackError],
          `agent deployment promotion failed and the previous deployment could not be restored; it remains at ${backup}`,
        );
      }
    }
    throw promotionError;
  }

  if (backup) {
    await rm(backup, { recursive: true, force: true });
    backup = undefined;
    targetMoved = false;
  }

  await rm(workspace, { recursive: true, force: true });
  workspace = undefined;
} catch (error) {
  if (!promoted && targetMoved && !rollbackAttempted) {
    rollbackAttempted = true;
    try {
      await rename(backup, target);
      targetMoved = false;
    } catch (rollbackError) {
      error = new AggregateError(
        [error, rollbackError],
        `agent deployment failed and the previous deployment could not be restored; it remains at ${backup}`,
      );
    }
  }

  const cleanupErrors = [];
  if (staging) {
    try {
      await rm(staging, { recursive: true, force: true });
    } catch (cleanupError) {
      cleanupErrors.push(cleanupError);
    }
  }
  if (workspace) {
    try {
      await rm(workspace, { recursive: true, force: true });
    } catch (cleanupError) {
      cleanupErrors.push(cleanupError);
    }
  }
  if (backup && !targetMoved) {
    try {
      await rm(backup, { recursive: true, force: true });
    } catch (cleanupError) {
      cleanupErrors.push(cleanupError);
    }
  }
  if (cleanupErrors.length > 0) {
    error = new AggregateError(
      [error, ...cleanupErrors],
      "agent deployment failed and temporary-path cleanup also failed",
    );
  }
  throw error;
}

process.stdout.write(`Prepared ordinary production agent deployment at ${target}\n`);
