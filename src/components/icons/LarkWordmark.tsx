import React from "react";

// Subtle text wordmark — replaces the upstream Handy SVG logo, which was
// visually loud for a utility app.
const LarkWordmark = ({ className = "" }: { className?: string }) => (
  // eslint-disable-next-line i18next/no-literal-string
  <span className={`font-semibold tracking-wide select-none ${className}`}>
    Lark
  </span>
);

export default LarkWordmark;
