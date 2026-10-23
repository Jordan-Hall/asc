import React from "react";

// Renders a greeting.
export function Hello(props: { name: string }) {
  return <div>{format(props.name)}</div>;
}

const format = (s: string) => s.trim();
