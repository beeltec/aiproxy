import tailwindcss from "@tailwindcss/vite";
import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import viteReact from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const apiTarget = process.env.AIPROXY_DEV_API ?? "http://localhost:8080";

export default defineConfig({
	resolve: { tsconfigPaths: true },
	// Inlined assets would become data: URLs, which the CSP blocks.
	build: { assetsInlineLimit: 0 },
	server: {
		proxy: {
			"/admin/api": apiTarget,
			"/v1": apiTarget,
			"/healthz": apiTarget,
		},
	},
	plugins: [
		tailwindcss(),
		tanstackStart({ spa: { enabled: true } }),
		viteReact(),
	],
});
