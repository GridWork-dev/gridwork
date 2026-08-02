"use client";

import { useEffect, useRef, useState } from "react";

const INSTALL_COMMAND = "cargo install gridwork";
const RESET_DELAY_MS = 1_200;

type CopyStatus = "Copy" | "Copied" | "Copy failed";

export function CopyCommand() {
  const [status, setStatus] = useState<CopyStatus>("Copy");
  const resetTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    return () => {
      if (resetTimer.current !== null) {
        clearTimeout(resetTimer.current);
      }
    };
  }, []);

  function scheduleReset() {
    if (resetTimer.current !== null) {
      clearTimeout(resetTimer.current);
    }

    resetTimer.current = setTimeout(() => {
      setStatus("Copy");
      resetTimer.current = null;
    }, RESET_DELAY_MS);
  }

  async function copyInstallCommand() {
    try {
      if (!navigator.clipboard) {
        throw new Error("Clipboard API unavailable");
      }

      await navigator.clipboard.writeText(INSTALL_COMMAND);
      setStatus("Copied");
    } catch {
      setStatus("Copy failed");
    } finally {
      scheduleReset();
    }
  }

  const accessibleLabel =
    status === "Copy"
      ? "Copy install command"
      : status === "Copied"
        ? "Install command copied"
        : "Copy install command failed";

  return (
    <span className="copy-command">
      <button
        type="button"
        className="copy-command__button"
        onClick={copyInstallCommand}
        aria-label={accessibleLabel}
      >
        <span aria-hidden="true">{status}</span>
      </button>
      <span
        className="landing-sr-only"
        role="status"
        aria-live="polite"
        aria-atomic="true"
      >
        {status}
      </span>
    </span>
  );
}
