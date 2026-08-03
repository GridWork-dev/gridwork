import { copyFile, mkdir } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const siteRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const publicRoot = resolve(siteRoot, "public");

await mkdir(publicRoot, { recursive: true });
await copyFile(resolve(siteRoot, "og.png"), resolve(publicRoot, "og.png"));
