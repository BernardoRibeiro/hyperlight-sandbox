import { readFile, writeFile } from "node:fs/promises";
import { resolve, dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const componentizeDir = resolve(here, "node_modules/@bytecodealliance/componentize-js/src");
const originalPath = resolve(componentizeDir, "componentize.js");
const patchedPath = resolve(componentizeDir, "componentize-toolbox.js");
let source = await readFile(originalPath, "utf8");
source = source.replace(
  /const finalBin = stubWasi\(\s*bin,\s*features,\s*witWorld,[\s\S]*?worldName\s*\);/,
  "const finalBin = bin; // keep WASI filesystem imports for the toolbox",
);
await writeFile(patchedPath, source);
const { componentize } = await import(pathToFileURL(patchedPath));
const guest = await readFile("toolbox.js", "utf8");
const { component } = await componentize(guest, {
  witPath: "../../wit/hyperlight-sandbox.wit",
  worldName: "hyperlight:sandbox/sandbox",
});
await writeFile("toolbox.wasm", component);
console.log(`Built toolbox.wasm (${component.byteLength} bytes)`);
