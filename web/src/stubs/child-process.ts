// Stands in for node:child_process in the browser bundle. bridgeNode (the only user) is never called here.
export const spawn = (): never => {
  throw new Error("child_process is not available in the browser");
};
export type ChildProcess = never;
