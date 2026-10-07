import { defineConfig } from "vite";

// The platform serves dist/ as static files behind its edge.
export default defineConfig({
  build: { outDir: "dist", sourcemap: true },
});
