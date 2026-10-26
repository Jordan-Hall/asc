// The signed-out front page: the desktop board or the phone board, by viewport width.
import { lazy, type ReactNode, Suspense } from "react";
import { useMobile } from "../lib/media";

const Desktop = lazy(() => import("./landing-desktop"));
const Mobile = lazy(() => import("./landing-mobile"));

export default function Landing(): ReactNode {
  const mobile = useMobile();
  return <Suspense fallback={null}>{mobile ? <Mobile /> : <Desktop />}</Suspense>;
}
