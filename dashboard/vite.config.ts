import preact from "@preact/preset-vite";
import { defineConfig } from "vite";
import { withZephyr } from "vite-plugin-zephyr";

// `npm run build` makes a plain static build in dist/. `npm run deploy` (mode "zephyr") also
// uploads it to Zephyr; that needs a Zephyr login.
export default defineConfig(({ mode }) => ({
  plugins: [preact(), ...(mode === "zephyr" ? [withZephyr()] : [])],
}));
