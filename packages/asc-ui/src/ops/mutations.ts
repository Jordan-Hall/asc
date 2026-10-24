// The mutations a view can start. Each maps to one asc verb run through `Bridge.Run` (SPEC §24.3).
export type MoveTarget =
  | { readonly kind: "change"; readonly id: string }
  | { readonly kind: "trunk"; readonly name: string };

export type Mutation =
  | { readonly kind: "move"; readonly change: string; readonly onto: MoveTarget }
  | { readonly kind: "reorder"; readonly stackIndex: number; readonly order: readonly string[] }
  | { readonly kind: "drop"; readonly change: string }
  | { readonly kind: "fold"; readonly change: string }
  | { readonly kind: "reword"; readonly change: string; readonly subject: string }
  | { readonly kind: "restack"; readonly change: string };

export function describeMutation(m: Mutation): string {
  switch (m.kind) {
    case "move":
      return `Move ${m.change} onto ${m.onto.kind === "change" ? m.onto.id : m.onto.name}`;
    case "reorder":
      return `Reorder stack ${m.stackIndex}`;
    case "drop":
      return `Drop ${m.change}`;
    case "fold":
      return `Fold ${m.change} into its parent`;
    case "reword":
      return `Reword ${m.change}`;
    case "restack":
      return `Restack ${m.change}`;
  }
}
