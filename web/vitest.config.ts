import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    // jsdom's default `about:blank` URL disables localStorage / sessionStorage
    // (they only activate for an http(s) origin). Pin a fake origin so
    // org-store can round-trip its X-Org-Id pin under test.
    environmentOptions: {
      jsdom: {
        url: "http://localhost/",
      },
    },
    include: ["src/**/*.{test,spec}.{ts,tsx}"],
  },
});
