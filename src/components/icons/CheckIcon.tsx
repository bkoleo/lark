import React from "react";

interface CheckIconProps {
  width?: number;
  height?: number;
  color?: string;
  className?: string;
}

const CheckIcon: React.FC<CheckIconProps> = ({
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
      strokeWidth="2.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      xmlns="http://www.w3.org/2000/svg"
      className={className}
    >
      <path d="M20 6 9 17l-5-5" />
    </svg>
  );
};

export default CheckIcon;
