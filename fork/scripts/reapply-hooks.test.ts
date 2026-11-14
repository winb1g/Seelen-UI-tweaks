// [fork] Tests for reapply-hooks.ts: `node --test fork/scripts/`.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { parseHooks } from "./fork-check.ts";
import { applyRecipe, insertAfterLast, insertBefore, RECIPES, replace } from "./reapply-hooks.ts";

const ROOT = new URL("../../", import.meta.url);
const read = (path: string) => readFileSync(new URL(path, ROOT), "utf8");
const HOOKS = parseHooks(read("FORK.md"));

test("every hooked file in FORK.md has a recipe, and every recipe is for a hooked file", () => {
  assert.deepEqual(Object.keys(RECIPES).sort(), [...new Set(HOOKS.map((h) => h.file))].sort());
});

test("recipes are no-ops on the hooked files", () => {
  for (const file of Object.keys(RECIPES)) {
    const source = read(file).replace(/\r\n/g, "\n");
    assert.equal(applyRecipe(file, source, HOOKS), source, file);
  }
});

test("replace swaps the single match, and refuses none or several", () => {
  const step = replace(/KEY = "[^"]*";/, "KEY = fork::KEY;");
  assert.equal(step.apply('a\nKEY = "abc$&";\nb'), "a\nKEY = fork::KEY;\nb");
  assert.throws(() => step.apply("nothing"), /found 0/);
  assert.throws(() => step.apply('KEY = "a";\nKEY = "b";'), /found 2/);
});

test("insertAfterLast inserts after the last match", () => {
  const step = insertAfterLast(/^mod \w+;\n/m, "\nmod fork;\n");
  assert.equal(
    step.apply("mod a;\n#[cfg(windows)]\nmod b;\n\nfn main() {}\n"),
    "mod a;\n#[cfg(windows)]\nmod b;\n\nmod fork;\n\nfn main() {}\n",
  );
  assert.throws(() => step.apply("fn main() {}\n"), /no match/);
});

test("insertBefore only searches after its scope", () => {
  const step = insertBefore(/\bfn setup\(/, /^ {4}Ok\(\(\)\)\n\}/m, "    fork::init();\n");
  const source =
    "fn a() {\n    Ok(())\n}\nfn setup() {\n    if x {\n        Ok(())\n    }\n    start();\n    Ok(())\n}\n";
  assert.equal(
    step.apply(source),
    "fn a() {\n    Ok(())\n}\nfn setup() {\n    if x {\n        Ok(())\n    }\n    start();\n    fork::init();\n    Ok(())\n}\n",
  );
  assert.throws(() => step.apply("fn other() {\n    Ok(())\n}\n"), /no match of/);
});

test("applyRecipe puts the release hooks back on upstream's updater", () => {
  const upstream = [
    'pub static SIGN_PUB_KEY: &str = "dW50cnVzdGVk";',
    "",
    '                "https://github.com/eythaann/Seelen-UI/releases/download/nightly/latest.json"',
    '                "https://github.com/eythaann/Seelen-UI/releases/latest/download/latest.json"',
    "",
  ].join("\r\n");
  const hooks = HOOKS.filter((h) => h.file === "src/background/utils/updater.rs");
  assert.equal(
    applyRecipe("src/background/utils/updater.rs", upstream, hooks),
    [
      "pub static SIGN_PUB_KEY: &str = crate::fork::release::SIGN_PUB_KEY;",
      "",
      "                crate::fork::release::NIGHTLY_ENDPOINT",
      "                crate::fork::release::RELEASE_ENDPOINT",
      "",
    ].join("\n"),
  );
});

test("applyRecipe fails when upstream moved the code a hook anchors to", () => {
  assert.throws(() => applyRecipe("src/background/utils/updater.rs", "fn moved() {}\n", HOOKS), /found 0/);
  assert.throws(() => applyRecipe("src/unknown.rs", "", HOOKS), /no recipe/);
});
