// [fork] Puts the fork's hooks back on upstream files after an upstream merge conflicts on them. Node built-ins only.
//
// Commands:
//   (none)        resolve the conflicted hook files of the merge in progress, exit 1 if any conflict is left
//   check <ref>   apply every recipe to <ref>'s version of its file and compare with the working tree, e.g.
//                 `check upstream/master` right after a sync, or after adding or changing a hook
//
// A hook file conflicts when upstream edits the lines next to a hook. Upstream files only carry hooks (FORK.md), so
// such a file is resolved by taking upstream's version and re-applying its recipe below. Files listed under "Upstream
// patches", files deleted upstream and files without a recipe are left for a human, as is any recipe whose anchor is
// gone (upstream moved the code). See fork/docs/ci.md.

import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";

import { firstCode, FORK_DOC, type Hook, parseHooks, PATCHES_SECTION, sectionRows } from "./fork-check.ts";

const SIGN_PUB_KEY = "crate::fork::release::SIGN_PUB_KEY";

/** One edit to an upstream file. */
export interface Step {
  /** Text whose presence means the step is already applied (applying twice is a no-op). */
  done: string;
  /** Throws when the code the step anchors to is gone. */
  apply(source: string): string;
}

/** Replaces the single match of `pattern`. */
export function replace(pattern: RegExp, replacement: string): Step {
  return {
    done: replacement,
    apply(source) {
      const count = [...source.matchAll(new RegExp(pattern.source, "g" + pattern.flags.replace("g", "")))].length;
      if (count !== 1) {
        throw new Error(`expected 1 match of ${pattern}, found ${count}`);
      }
      return source.replace(pattern, () => replacement);
    },
  };
}

/** Inserts `text` right after the last match of `pattern`. */
export function insertAfterLast(pattern: RegExp, text: string): Step {
  return {
    done: text.trim(),
    apply(source) {
      const last = [...source.matchAll(new RegExp(pattern.source, "g" + pattern.flags.replace("g", "")))].at(-1);
      if (!last) {
        throw new Error(`no match of ${pattern}`);
      }
      const at = last.index + last[0].length;
      return source.slice(0, at) + text + source.slice(at);
    },
  };
}

/** Inserts `text` right before the first match of `pattern` that follows the first match of `after`. */
export function insertBefore(after: RegExp, pattern: RegExp, text: string): Step {
  return {
    done: text.trim(),
    apply(source) {
      const scope = source.match(after);
      if (scope?.index === undefined) {
        throw new Error(`no match of ${after}`);
      }
      const match = source.slice(scope.index).match(pattern);
      if (match?.index === undefined) {
        throw new Error(`no match of ${pattern} after ${after}`);
      }
      const at = scope.index + match.index;
      return source.slice(0, at) + text + source.slice(at);
    },
  };
}

/** How to re-apply the hooks of each file in FORK.md's hooks table (the tests keep both lists in sync). */
export const RECIPES: Record<string, Step[]> = {
  "src/background/main.rs": [
    insertAfterLast(/^mod \w+;\n/m, '\n#[path = "../../fork/background/mod.rs"]\nmod fork;\n'),
    insertBefore(/\bfn setup\(/, /^ {4}Ok\(\(\)\)\n\}/m, "    fork::init();\n"),
  ],
  "src/ui/svelte/weg/components/items/UserApplicationItem.svelte": [
    insertAfterLast(/^ {2}import .*;\n/m, '  import { appVolumeControls } from "fork/weg/appVolume.svelte.ts";\n'),
    insertBefore(/class="weg-item"/, /^[ \t]*>\n/m, "    {@attach appVolumeControls(() => item, () => windows)}\n"),
  ],
  "src/background/utils/updater.rs": [
    replace(/pub static SIGN_PUB_KEY: &str = "[^"]*";/, `pub static SIGN_PUB_KEY: &str = ${SIGN_PUB_KEY};`),
    replace(
      /"https:\/\/github\.com\/[^"]+\/releases\/download\/nightly\/latest\.json"/,
      "crate::fork::release::NIGHTLY_ENDPOINT",
    ),
    replace(
      /"https:\/\/github\.com\/[^"]+\/releases\/latest\/download\/latest\.json"/,
      "crate::fork::release::RELEASE_ENDPOINT",
    ),
  ],
  "src/background/utils/integrity/checksums.rs": [
    replace(/const MINISIGN_PUBLIC_KEY: &str = "[^"]*";/, `const MINISIGN_PUBLIC_KEY: &str = ${SIGN_PUB_KEY};`),
  ],
};

/** Applies `file`'s recipe to `source`, then checks that every FORK.md marker of the file is present. */
export function applyRecipe(file: string, source: string, hooks: Hook[]): string {
  const steps = RECIPES[file];
  if (!steps) {
    throw new Error("no recipe in fork/scripts/reapply-hooks.ts");
  }
  let result = source.replace(/\r\n/g, "\n");
  for (const step of steps) {
    if (!result.includes(step.done)) {
      result = step.apply(result);
    }
  }
  const missing = hooks.filter((h) => h.file === file && !result.includes(h.marker));
  if (missing.length > 0) {
    throw new Error(`recipe applied but hook still missing: ${missing.map((h) => `\`${h.marker}\``).join(", ")}`);
  }
  return result;
}

function git(...args: string[]): string {
  return execFileSync("git", args, { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 });
}

function lines(output: string): string[] {
  return output.split("\n").map((l) => l.trim()).filter(Boolean);
}

/** Resolves the conflicted hook files of the merge in progress. Returns false when conflicts remain. */
function resolve(): boolean {
  const doc = readFileSync(FORK_DOC, "utf8");
  const hooks = parseHooks(doc);
  const patched = new Set(sectionRows(doc, PATCHES_SECTION).map((row) => firstCode(row[0])));

  const resolved: string[] = [];
  const left: string[] = [];
  for (const file of lines(git("diff", "--name-only", "--diff-filter=U"))) {
    const stages = lines(git("ls-files", "-u", "--", file)).map((l) => l.split(/\s+/)[2]);
    let reason: string;
    if (patched.has(file)) {
      reason = `listed under "${PATCHES_SECTION}"`;
    } else if (!RECIPES[file]) {
      reason = "no recipe (not a hook file)";
    } else if (!stages.includes("3")) {
      reason = "deleted or moved upstream, the hooks need a new home";
    } else {
      try {
        writeFileSync(file, applyRecipe(file, git("show", `:3:${file}`), hooks));
        git("add", "--", file);
        resolved.push(file);
        continue;
      } catch (err) {
        reason = (err as Error).message;
      }
    }
    left.push(`- \`${file}\`: ${reason}`);
  }

  if (resolved.length > 0) {
    console.log("Fork hooks re-applied on upstream's version of:\n");
    console.log(resolved.map((f) => `- \`${f}\``).join("\n") + "\n");
  }
  if (left.length > 0) {
    console.log("Still conflicted, resolve by hand:\n");
    console.log(left.join("\n") + "\n");
  }
  return left.length === 0;
}

/** Compares each recipe applied to `ref`'s version of its file with the working tree. */
function check(ref: string | undefined): boolean {
  if (!ref) {
    throw new Error("usage: check <ref>");
  }
  const hooks = parseHooks(readFileSync(FORK_DOC, "utf8"));
  let ok = true;
  for (const file of Object.keys(RECIPES)) {
    try {
      const expected = applyRecipe(file, git("show", `${ref}:${file}`), hooks);
      const actual = readFileSync(file, "utf8").replace(/\r\n/g, "\n");
      if (expected !== actual) {
        const a = actual.split("\n");
        const line = expected.split("\n").findIndex((l, i) => l !== a[i]);
        throw new Error(`differs from the working tree at line ${line + 1}`);
      }
      console.log(`✓ ${file}`);
    } catch (err) {
      console.error(`::error file=${file}::${(err as Error).message}`);
      ok = false;
    }
  }
  return ok;
}

function main(argv: string[]): void {
  const [command, arg] = argv;
  const ok = command === "check" ? check(arg) : command === undefined ? resolve() : false;
  if (command !== undefined && command !== "check") {
    console.error("usage: reapply-hooks.ts [check <ref>]");
  }
  process.exit(ok ? 0 : 1);
}

// Run only as a CLI, not when imported by the tests.
if (process.argv[1]?.endsWith("reapply-hooks.ts")) {
  main(process.argv.slice(2));
}
