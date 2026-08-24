import { build } from "esbuild";
import { cp, mkdir, rm } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "..");
const dist = resolve(root, "dist");

await rm(dist, { recursive: true, force: true });
await mkdir(dist, { recursive: true });
await cp(resolve(root, "public"), dist, { recursive: true });

await build({
  absWorkingDir: root,
  bundle: true,
  entryPoints: {
    background: "src/background/index.ts",
    content: "src/content/index.ts",
    popup: "src/popup/index.ts",
    offscreen: "src/offscreen/index.ts"
  },
  outdir: dist,
  format: "esm",
  target: ["chrome120"],
  sourcemap: true,
  legalComments: "none",
  logLevel: "info"
});

await build({
  absWorkingDir: root,
  bundle: true,
  entryPoints: {
    popup: "src/popup/styles.css"
  },
  outdir: dist,
  minify: false,
  sourcemap: true,
  logLevel: "info"
});
