// The signed-in home: what needs you, your stacks and what happened today, read from the
// repositories this browser has opened (asc has no "my repositories" RPC yet). The desktop board
// and the phone board are two layouts of the same data.
import { lazy, type ReactNode, Suspense } from "react";
import { useMobile } from "../lib/media";

const Desktop = lazy(() => import("./dashboard-desktop"));
const Mobile = lazy(() => import("./dashboard-mobile"));

export default function Dashboard(): ReactNode {
  const mobile = useMobile();
  return <Suspense fallback={null}>{mobile ? <Mobile /> : <Desktop />}</Suspense>;
}
