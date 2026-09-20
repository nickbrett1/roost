import { sveltekit } from "@sveltejs/kit/vite";
import { svelteTesting } from "@testing-library/svelte/vite";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [sveltekit(), svelteTesting()],
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["src/test-setup.js"],
    reporter: ["default", "junit"],
    outputFile: {
      junit: "./reports/junit.xml",
    },
    coverage: {
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
