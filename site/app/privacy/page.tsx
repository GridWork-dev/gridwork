import type { Metadata } from "next";

export const metadata: Metadata = {
  title: "Privacy — GridWork",
  description:
    "What this site measures: cookieless, aggregate page counts and nothing else.",
  alternates: { canonical: "/privacy" },
};

export default function PrivacyPage() {
  return (
    <main id="main-content" className="landing-shell landing-truth">
      <div className="landing-section-heading">
        <h1>Privacy</h1>
        <p>
          This site uses Plausible for analytics: cookieless, aggregate page
          counts only, with no personal data collected and no cross-site
          tracking — so no consent banner is required.
        </p>
      </div>
    </main>
  );
}
