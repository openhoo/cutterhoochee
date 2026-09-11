import { spawnSync } from "node:child_process";
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

run(["--filter=@cutterhoochee/shared", "build"]);
run(["--filter=@cutterhoochee/app", "build"]);
