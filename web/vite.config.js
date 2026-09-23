import { execFileSync } from "node:child_process";

import { svelte } from "@sveltejs/vite-plugin-svelte";
import { defineConfig } from "vitest/config";

/// The build stamp: baked into the bundle at build time so the running page can
/// say which build it is. That is the one question a stale browser tab cannot
/// answer for itself, and the reason this is not fetched from the API (a stale
/// page would happily fetch a fresh answer and still be stale).
///
/// CI passes the values in (see the Dockerfile build args); a local build falls
/// back to the checkout's own HEAD so the dev loop shows something true.
function gitHead() {
  try {
    const out = execFileSync("git", ["log", "-1", "--format=%h%n%s"], {
      cwd: process.cwd(),
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    });
    return out.trim().split("\n");
  } catch {
    return [];
  }
}

const [gitSha = "", gitSubject = ""] = gitHead();
const buildTime = process.env.BUILD_TIME || new Date().toISOString();
const buildCommit = process.env.BUILD_COMMIT || gitSha || "unknown";
// Only the subject: a commit body in the footer would be noise, and it would
// blow the width on a phone.
const buildMessage = (process.env.BUILD_MESSAGE || gitSubject || "").split("\n")[0];
const buildNumber = process.env.BUILD_NUMBER || "";

export default defineConfig({
  plugins: [svelte()],
  define: {
    __BUILD_TIME__: JSON.stringify(buildTime),
    __BUILD_COMMIT__: JSON.stringify(buildCommit),
    __BUILD_MESSAGE__: JSON.stringify(buildMessage),
    __BUILD_NUMBER__: JSON.stringify(buildNumber),
  },
  test: {
    passWithNoTests: true,
    reporter: ["default"],
    coverage: {
      // Measure only the framework-free logic in src/lib. Components are
      // exercised in the browser, not by a jsdom harness here.
      include: ["src/lib/**/*.js"],
      reporter: ["lcov", "text"],
      thresholds: {
        statements: 80,
        branches: 50,
        functions: 80,
        lines: 80,
      },
    },
  },
});
