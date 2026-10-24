// Jotai state. The store is created per view host, never at module scope, so tests and several
// webviews never share atoms. React binds with `<Provider store={store}>` in the view units.
import { atom, type createStore } from "jotai/vanilla";
import type { OpRunner, QueueSnapshot } from "../ops/queue";
import { OpsQueue } from "../ops/queue";
import { EMPTY_STACKS, type Stacks } from "./model";

/** The last model confirmed by the server (`Workspace.Stacks` after a `Watch` event). */
export const confirmedStacksAtom = atom<Stacks>(EMPTY_STACKS);
/** The confirmed model plus the previews of live ops. Views render this one. */
export const predictedStacksAtom = atom<Stacks>(EMPTY_STACKS);
export const queueSnapshotAtom = atom<QueueSnapshot>({
  root: "",
  queued: [],
  running: undefined,
  optimistic: [],
  finished: [],
});

export type AscStore = ReturnType<typeof createStore>;

export interface OpsController {
  readonly queue: OpsQueue;
  /** Feed a fresh server model (after `Watch`). */
  confirm(model: Stacks): void;
  dispose(): void;
}

/** Wire one root's ops queue to a store. */
export function bindOpsQueue(store: AscStore, root: string, run: OpRunner): OpsController {
  const queue = new OpsQueue({
    root,
    run,
    onChange: (snapshot, predicted) => {
      store.set(queueSnapshotAtom, snapshot);
      store.set(predictedStacksAtom, predicted);
    },
  });
  store.set(queueSnapshotAtom, queue.snapshot());
  return {
    queue,
    confirm(model) {
      store.set(confirmedStacksAtom, model);
      queue.confirm(model);
    },
    dispose: () => queue.dispose(),
  };
}
