import React from "react";

interface RecordIconProps {
  width?: number;
  height?: number;
  color?: string;
  className?: string;
}

const RecordIcon: React.FC<RecordIconProps> = ({
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
      <circle cx="12" cy="12" r="6" />
    </svg>
  );
};

export default RecordIcon;
