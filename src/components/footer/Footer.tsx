import React, { useState, useEffect } from "react";
import { getVersion } from "@tauri-apps/api/app";

const Footer: React.FC = () => {
  const [version, setVersion] = useState("");

  useEffect(() => {
    const fetchVersion = async () => {
      try {
        const appVersion = await getVersion();
        setVersion(appVersion);
      } catch (error) {
        console.error("Failed to get app version:", error);
        setVersion("0.1.2");
      }
    };

    fetchVersion();
  }, []);

  return (
    <div className="w-full border-t border-mid-gray/20 pt-3">
      <div className="flex justify-end items-center text-xs px-4 pb-3 text-text/40">
        {/* eslint-disable-next-line i18next/no-literal-string */}
        <span>v{version}</span>
      </div>
    </div>
  );
};

export default Footer;
