// `@asc/build-cache`: the build-level cache of docs/designs/build-cache.md §4.2 for Rsbuild,
// Rslib and Vite. Import the plugin of the tool from `@asc/build-cache/rsbuild` or
// `@asc/build-cache/vite`; this entry gives both.

export type { AscBuildCacheOptions } from "./core.js";
export { pluginAscBuildCache } from "./rsbuild.js";
export { ascBuildCache } from "./vite.js";
