import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { createRelativeLink } from "fumadocs-ui/mdx";
import { DocsBody, DocsDescription, DocsPage, DocsTitle } from "fumadocs-ui/layouts/docs/page";

import { getMDXComponents } from "@/components/mdx";
import sourceMap from "@/content/source-map.json";
import { source } from "@/lib/source";

export const dynamicParams = false;

const canonicalSourceBase = "https://github.com/GridWork-dev/gridwork/blob/main/";

function canonicalSourceHref(path: string): string {
  const encodedPath = path.split("/").map(encodeURIComponent).join("/");
  return `${canonicalSourceBase}${encodedPath}`;
}

export default async function Page(props: PageProps<"/docs/[[...slug]]">) {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  const MDX = page.data.body;
  const destination = `content/docs/${page.path}`;
  const provenance = sourceMap.pages.find((entry) => entry.destination === destination);
  if (!provenance) {
    throw new Error(`missing canonical sources for ${destination}`);
  }

  return (
    <DocsPage toc={page.data.toc} full={page.data.full}>
      <DocsTitle>{page.data.title}</DocsTitle>
      <DocsDescription>{page.data.description}</DocsDescription>
      <DocsBody>
        <MDX
          components={getMDXComponents({
            a: createRelativeLink(source, page),
          })}
        />
      </DocsBody>
      <footer className="canonical-sources" aria-label="Canonical sources">
        <span>Canonical sources</span>
        <ul>
          {provenance.sources.map((canonicalSource) => (
            <li key={canonicalSource.path}>
              <a href={canonicalSourceHref(canonicalSource.path)}>{canonicalSource.path}</a>
            </li>
          ))}
        </ul>
      </footer>
    </DocsPage>
  );
}

export function generateStaticParams() {
  return source.generateParams();
}

export async function generateMetadata(props: PageProps<"/docs/[[...slug]]">): Promise<Metadata> {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  return {
    title: page.data.title,
    description: page.data.description,
    // Without this every docs page inherits the layout's canonical and points
    // search engines at the landing page instead of itself.
    alternates: { canonical: page.url },
  };
}
