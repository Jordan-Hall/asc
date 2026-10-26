// "Run target" (SPEC 24.4.7): a `TaskProvider` of type `asc` with one task per affected project and
// target. The command is the argv asc serves for the target (API addition A4), run through
// `ShellExecution(cmd, args)` with the project root as cwd in the terminal panel. asc runs and
// schedules nothing. The argv is repository content, so tasks exist only in a trusted workspace and
// the cwd must stay inside the workspace root.
import { posix, win32 } from "node:path";
import type * as vscodeTypes from "vscode";
import { type AffectedModel, equivalentCommand } from "./views/affected";

export const TASK_TYPE = "asc";

export interface TaskSpec {
  /** The asc root the project belongs to. */
  root: string;
  project: string;
  projectName: string;
  target: string;
  command: string;
  args: string[];
  /** Absolute cwd: the project root inside the workspace root. */
  cwd: string;
  /** Shown in the task detail. */
  detail: string;
}

/**
 * Joins a repo-relative project root onto the workspace root; undefined when the result would leave
 * it (an absolute path, a drive, `..`, or a backslash trick).
 */
export function projectCwd(workspaceRoot: string, rel: string): string | undefined {
  const r = rel.replaceAll("\\", "/");
  if (r === "" || r === ".") return workspaceRoot;
  if (r.startsWith("/") || /^[A-Za-z]:/.test(r) || r.includes("\0")) return undefined;
  if (posix.normalize(r).split("/").includes("..")) return undefined;
  const sep = workspaceRoot.includes("\\") ? win32 : posix;
  return sep.join(workspaceRoot, ...r.split("/").filter((s) => s !== "" && s !== "."));
}

/** One spec per affected project and target; a project without a target definition gets none. */
export function buildTaskSpecs(
  root: string,
  projects: readonly {
    id: string;
    name: string;
    root: string;
    targets: readonly { name: string; argv: readonly string[] }[];
  }[],
): TaskSpec[] {
  const out: TaskSpec[] = [];
  for (const p of projects) {
    const cwd = projectCwd(root, p.root);
    if (cwd === undefined) continue;
    for (const t of p.targets) {
      const [command, ...args] = t.argv;
      if (!command) continue;
      out.push({
        root,
        project: p.id,
        projectName: p.name || p.id,
        target: t.name,
        command,
        args,
        cwd,
        detail: `${command} ${args.join(" ")}  (like ${equivalentCommand(t.name)})`.trim(),
      });
    }
  }
  return out;
}

export interface AscTaskDefinition {
  type: "asc";
  root: string;
  project: string;
  target: string;
}

function toTask(vscode: typeof vscodeTypes, s: TaskSpec): vscodeTypes.Task {
  const def: AscTaskDefinition = {
    type: TASK_TYPE,
    root: s.root,
    project: s.project,
    target: s.target,
  };
  const task = new vscode.Task(
    def,
    vscode.TaskScope.Workspace,
    `${s.projectName}: ${s.target}`,
    "asc",
    new vscode.ShellExecution(
      s.command,
      s.args.map((value) => ({ value, quoting: vscode.ShellQuoting.Strong })),
      { cwd: s.cwd },
    ),
  );
  task.detail = s.detail;
  task.presentationOptions = {
    reveal: vscode.TaskRevealKind.Always,
    panel: vscode.TaskPanelKind.Dedicated,
  };
  return task;
}

export function registerTasks(
  vscode: typeof vscodeTypes,
  models: readonly AffectedModel[],
  log: (line: string) => void,
): { dispose(): void } {
  const specsFor = async (): Promise<TaskSpec[]> => {
    if (!vscode.workspace.isTrusted) return [];
    const all: TaskSpec[] = [];
    for (const m of models) {
      try {
        all.push(...buildTaskSpecs(m.root, (await m.current()).projects));
      } catch (e) {
        log(`asc tasks: ${String(e)}`);
      }
    }
    return all;
  };
  const reg = vscode.tasks.registerTaskProvider(TASK_TYPE, {
    async provideTasks() {
      return (await specsFor()).map((s) => toTask(vscode, s));
    },
    async resolveTask(task) {
      const d = task.definition as Partial<AscTaskDefinition>;
      if (
        typeof d.root !== "string" ||
        typeof d.project !== "string" ||
        typeof d.target !== "string"
      ) {
        return undefined;
      }
      const spec = (await specsFor()).find(
        (s) => s.root === d.root && s.project === d.project && s.target === d.target,
      );
      return spec ? toTask(vscode, spec) : undefined;
    },
  });
  return { dispose: () => reg.dispose() };
}
