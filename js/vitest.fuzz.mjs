import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["test/fuzz.test.ts"],
    testTimeout: 120_000,
    env: {
      FUZZ_CASES: process.env.FUZZ_CASES ?? "256",
    },
  },
});
