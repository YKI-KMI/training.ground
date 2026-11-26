// `npm run tauri dev:cdp` → launches `tauri dev` with WebView2 remote
// debugging on port 9333 so e2e.mjs can drive the real window over CDP.
import { spawn } from "node:child_process";

const PORT = 9333;
const isWin = process.platform === "win32";
const child = spawn(
  "npx",
  ["tauri", "dev"],
  {
    stdio: "inherit",
    shell: isWin, // Node ≥22 forbids spawning .cmd without a shell
    env: {
      ...process.env,
      WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${PORT}`,
    },
  },
);
child.on("exit", (code) => process.exit(code ?? 1));
