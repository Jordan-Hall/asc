#!/usr/bin/env node
// `asc-rstest [--asc-name N] [--asc-output PATH]... [--asc-input PATH]... [--asc-env NAME]...
// [--asc-no-remote] [--asc-off] run [rstest options]`: see rstest.ts.

import { runRstest } from "./rstest.js";

try {
  process.exitCode = await runRstest(process.argv.slice(2));
} catch (error) {
  process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
  process.exitCode = 1;
}
