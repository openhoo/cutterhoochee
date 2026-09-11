import { spawn, spawnSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const pnpm = process.platform === "win32" ? "pnpm.cmd" : "pnpm";
const env = {
  ...process.env,
  NODE_ENV: "development",
  npm_config_production: "false",
  PNPM_CONFIG_PRODUCTION: "false",
};
const shared = spawnSync(pnpm, ["--filter=@cutterhoochee/shared", "build"], {
  cwd: root,
  env,
  stdio: "inherit",
  shell: false,
});
if (shared.error) throw new Error(`pnpm could not be started: ${shared.error.message}`);
if (shared.status !== 0) process.exit(shared.status ?? 1);

const server = spawn(pnpm, ["--filter=@cutterhoochee/app", "dev"], {
  cwd: root,
  env,
  stdio: "inherit",
  shell: false,
});
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.on(signal, () => server.kill(signal));
}
server.on("error", (error) => {
  console.error(`pnpm could not be started: ${error.message}`);
  process.exitCode = 1;
});
server.on("exit", (code, signal) => {
  process.exitCode = code ?? (signal ? 1 : 0);
});
