// `@asc/build-cache`: the build-level cache of docs/designs/build-cache.md §4.2 for Rsbuild,
// Rslib, Vite and Rstest. Import the plugin of the tool from `@asc/build-cache/rsbuild` or
// `@asc/build-cache/vite`, and the Rstest runner from `@asc/build-cache/rstest` (or run the
// `asc-rstest` command); this entry gives all three.

export type { AscBuildCacheOptions } from "./core.js";
export { pluginAscBuildCache } from "./rsbuild.js";
export { type AscRstestOptions, runRstest } from "./rstest.js";
export { ascBuildCache } from "./vite.js";
