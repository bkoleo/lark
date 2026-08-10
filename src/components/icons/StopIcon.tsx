import React from "react";

interface StopIconProps {
  width?: number;
  height?: number;
  color?: string;
  className?: string;
}

const StopIcon: React.FC<StopIconProps> = ({
  width = 24,
  height = 24,
  color = "currentColor",
  className = "",
}) => {
  return (
    <svg
      width={width}
      height={height}
      viewBox="0 0 24 24"
      fill="none"
      stroke={color}
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      xmlns="http://www.w3.org/2000/svg"
      className={className}
    >
      <rect x="7" y="7" width="10" height="10" rx="2" />
    </svg>
  );
};

export default StopIcon;
