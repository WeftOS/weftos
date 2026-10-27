import { source } from '@/lib/source';
import {
  DocsPage,
  DocsBody,
  DocsDescription,
  DocsTitle,
} from 'fumadocs-ui/page';
import { notFound } from 'next/navigation';
import defaultMdxComponents from 'fumadocs-ui/mdx';
import { findNeighbour } from 'fumadocs-core/page-tree';
import type { DocData } from 'fumadocs-mdx/runtime/types';

export default async function Page(props: {
  params: Promise<{ slug?: string[] }>;
}) {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  const data = page.data as typeof page.data & DocData;
  const MDX = data.body;
  const neighbours = findNeighbour(source.pageTree, page.url);
  const filePath = page.path;

  return (
    <DocsPage
      toc={data.toc}
      full={page.data.full}
      footer={{ items: neighbours }}
      editOnGithub={{
        owner: 'WeftOS',
        repo: 'weftos',
        sha: 'master',
        path: `docs/src/content/docs/${filePath}`,
      }}
    >
      <DocsTitle>{page.data.title}</DocsTitle>
      <DocsDescription>{page.data.description}</DocsDescription>
      <DocsBody>
        <MDX components={{ ...defaultMdxComponents }} />
      </DocsBody>
    </DocsPage>
  );
}

export function generateStaticParams() {
  return source.generateParams();
}

export async function generateMetadata(props: {
  params: Promise<{ slug?: string[] }>;
}) {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();
  return { title: page.data.title, description: page.data.description };
}
