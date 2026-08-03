import type { BaseLayoutProps } from "fumadocs-ui/layouts/shared";

export function baseOptions(): BaseLayoutProps {
  return {
    nav: {
      title: "GridWork",
      url: "/",
      transparentMode: "none",
    },
    links: [
      {
        text: "Home",
        url: "/",
        active: "none",
      },
      {
        text: "GitHub",
        url: "https://github.com/GridWork-dev/gridwork",
        external: true,
        active: "none",
      },
    ],
    searchToggle: { enabled: false },
    themeSwitch: { enabled: false },
  };
}
