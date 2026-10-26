// The app root: the product page for a signed-out visitor, the dashboard once there is a token.
// Both are lazy chunks; the route loader (routes.ts) fetches the one that will be shown together
// with this file, so there is no second round trip.
import { lazy, type ReactNode } from "react";
import { useApp } from "../api/queries";
import { useToken } from "../lib/auth";

const Landing = lazy(() => import("./landing"));
const Dashboard = lazy(() => import("./dashboard"));

export default function Home(): ReactNode {
  const { auth } = useApp();
  return useToken(auth) ? <Dashboard /> : <Landing />;
}
