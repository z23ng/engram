// Fails the build when the entry script grows past the limit; heavy views must load lazily.
import { readFileSync, statSync } from "node:fs";

const LIMIT = 500 * 1024;
const html = readFileSync("dist/index.html", "utf8");
const entry = html.match(/<script type="module"[^>]*src="\/([^"]+)"/)?.[1];
if (!entry) {
  console.error("check-bundle: no module script in dist/index.html");
  process.exit(1);
}
const size = statSync(`dist/${entry}`).size;
const kb = (n) => `${(n / 1024).toFixed(1)} kB`;
if (size > LIMIT) {
  console.error(`check-bundle: ${entry} is ${kb(size)}, limit ${kb(LIMIT)}`);
  process.exit(1);
}
console.log(`check-bundle: ${entry} is ${kb(size)} (limit ${kb(LIMIT)})`);
