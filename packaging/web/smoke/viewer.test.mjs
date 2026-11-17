// Tests of viewer.mjs's argument handling (node --test viewer.test.mjs): a run that
// cannot do what was asked must stop with the usage (exit 2), not go on and look
// like a pass. Each case ends before a browser or server is started.

import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const VIEWER = fileURLToPath(new URL("./viewer.mjs", import.meta.url));
const run = (...args) => spawnSync(process.execPath, [VIEWER, ...args], { encoding: "utf8", timeout: 30_000 });

for (const [what, args, says] of [
  ["an empty --core", ["--dist", "dist/web", "--core", ""], "--core needs a value"],
  ["an empty --dist", ["--dist", ""], "--dist needs a value"],
  ["--core with no value", ["--dist", "dist/web", "--core"], "--core needs a value"],
  ["--core followed by another option", ["--core", "--dist", "dist/web"], "--core needs a value"],
  ["an option given twice", ["--dist", "a", "--dist", "b"], "--dist given twice"],
  ["an unknown option", ["--dits", "dist/web"], 'unknown argument "--dits"'],
]) {
  test(`viewer.mjs refuses ${what}`, () => {
    const got = run(...args);
    assert.equal(got.status, 2, got.stderr);
    assert.match(got.stderr, new RegExp(says.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
    assert.match(got.stderr, /usage: node viewer\.mjs/);
  });
}
