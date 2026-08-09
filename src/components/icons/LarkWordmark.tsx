import React from "react";

// The lark, drawn filled rather than as an outline: this renders at roughly the
// line height of the surrounding text, and an outline drawing goes hollow below
// about 32px. The full outline version lives in the app icon.
const LarkMark = ({ className = "" }: { className?: string }) => (
  <svg
    viewBox="0 0 100 100"
    className={className}
    aria-hidden="true"
    focusable="false"
  >
    <path
      fill="currentColor"
      stroke="currentColor"
      strokeWidth="6"
      strokeLinejoin="round"
      strokeLinecap="round"
      d="M11 36.6 C 20 33, 26 31, 31 30 C 31.4 21, 37.4 14, 44.5 14
         C 46.4 7.6, 53.4 4.6, 58.6 6 C 55.2 12, 50.2 16, 49 20
         C 58.4 24, 64.4 33, 63.2 42 C 73.6 50, 77.4 63, 68.2 73
         C 78 79.6, 86 85.6, 94 92.6 C 84.4 89, 74.6 83.4, 66.4 77
         C 54 82, 38.6 77, 32.8 66 C 28.6 57, 29.6 46.6, 33 41
         C 25 40, 18 38.6, 11 36.6 Z"
    />
  </svg>
);

// Mark plus name. Sized in em so it tracks whatever type scale it is dropped into.
const LarkWordmark = ({ className = "" }: { className?: string }) => (
  <span className={`inline-flex items-center gap-[0.4em] select-none ${className}`}>
    <LarkMark className="w-[1.1em] h-[1.1em] shrink-0" />
    {/* eslint-disable-next-line i18next/no-literal-string */}
    <span className="font-semibold tracking-wide">Lark</span>
  </span>
);

export { LarkMark };
export default LarkWordmark;
