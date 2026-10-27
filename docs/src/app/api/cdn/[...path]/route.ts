import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { type NextRequest, NextResponse } from 'next/server';

export const runtime = 'nodejs';

/**
 * Server-side proxy for CDN assets (WASM, KB).
 *
 * GitHub Releases redirects to release-assets.githubusercontent.com which
 * lacks CORS headers. This route handler fetches server-side and streams
 * the response back with correct Content-Type and caching headers.
 *
 * Routes (rolling — production default):
 *   /api/cdn/wasm/clawft_wasm.js
 *   /api/cdn/wasm/clawft_wasm_bg.wasm
 *   /api/cdn/kb/weftos-docs.rvf
 *
 * Routes (SHA snapshots — WEFT-454 rollback / pin):
 *   /api/cdn/wasm/clawft_wasm-{sha}.js
 *   /api/cdn/wasm/clawft_wasm-{sha}.wasm
 *   /api/cdn/kb/weftos-docs-{sha}.rvf
 *
 * Optional env:
 *   CDN_ORIGIN — override GitHub Releases base URL
 *   CDN_SHA    — when set, rewrite rolling filenames to SHA-stamped siblings
 *
 * Lifecycle / cache-bust: docs/deployment/cdn.md
 */

const CDN_ORIGIN =
  process.env.CDN_ORIGIN ||
  'https://github.com/WeftOS/weftos/releases/download/cdn-assets';

/** Optional pin: rewrite rolling names to clawft_wasm-{sha}.* siblings.
 *  Snapshots are published under the 12-char short SHA (WEFT-454). */
const CDN_SHA_RAW = (process.env.CDN_SHA || '').trim().toLowerCase();
const CDN_SHA =
  CDN_SHA_RAW.length >= 12 ? CDN_SHA_RAW.slice(0, 12) : CDN_SHA_RAW;

const MIME_TYPES: Record<string, string> = {
  '.js': 'application/javascript',
  '.wasm': 'application/wasm',
  '.rvf': 'application/octet-stream',
  '.json': 'application/json',
};

// Rolling filenames allowed without a SHA pin.
const ROLLING = new Set([
  'wasm/clawft_wasm.js',
  'wasm/clawft_wasm_bg.wasm',
  'kb/weftos-docs.rvf',
  // Audit trail (WEFT-454)
  'cdn-manifest.json',
]);

// SHA-stamped: wasm/clawft_wasm-{7–40 hex}.(js|wasm)
//              kb/weftos-docs-{7–40 hex}.rvf
const SNAPSHOT_RE =
  /^(?:wasm\/clawft_wasm|kb\/weftos-docs)-[0-9a-f]{7,40}\.(?:js|wasm|rvf)$/;

function isAllowed(filePath: string): boolean {
  if (ROLLING.has(filePath)) return true;
  return SNAPSHOT_RE.test(filePath);
}

/**
 * Map a logical proxy path to the flat GitHub Release asset filename.
 * When CDN_SHA is set, rolling names resolve to the stamped siblings so a
 * docs deployment can pin without waiting for edge-cache expiry.
 */
function toUpstreamFilename(filePath: string): string {
  const base = filePath.includes('/')
    ? filePath.slice(filePath.lastIndexOf('/') + 1)
    : filePath;

  if (!CDN_SHA) return base;

  // Only rewrite known rolling names; leave explicit snapshots alone.
  if (base === 'clawft_wasm_bg.wasm') return `clawft_wasm-${CDN_SHA}.wasm`;
  if (base === 'clawft_wasm.js') return `clawft_wasm-${CDN_SHA}.js`;
  if (base === 'weftos-docs.rvf') return `weftos-docs-${CDN_SHA}.rvf`;
  return base;
}

export async function GET(
  _req: NextRequest,
  { params }: { params: Promise<{ path: string[] }> },
) {
  const { path } = await params;
  const filePath = path.join('/');

  if (!isAllowed(filePath)) {
    return NextResponse.json({ error: 'not found' }, { status: 404 });
  }

  const ext = filePath.includes('.')
    ? filePath.substring(filePath.lastIndexOf('.'))
    : '';
  const contentType = MIME_TYPES[ext] || 'application/octet-stream';

  // Local public/ copy wins (scripts/pull-assets.sh --local, scripts/build-kb.sh).
  // Production deploys gitignore these files and fall through to GitHub Releases.
  try {
    const localPath = join(process.cwd(), 'public', filePath);
    const body = await readFile(localPath);
    return new NextResponse(body, {
      status: 200,
      headers: {
        'Content-Type': contentType,
        'Cache-Control': 'public, max-age=0, must-revalidate',
        'Access-Control-Allow-Origin': '*',
        'X-Weft-CDN-Source': 'local',
      },
    });
  } catch {
    // not on disk — proxy the rolling GitHub Release
  }

  // The GitHub Release URL uses the filename directly (no subdirectories).
  const filename = toUpstreamFilename(filePath);
  const upstream = `${CDN_ORIGIN}/${filename}`;

  const resp = await fetch(upstream, { redirect: 'follow' });
  if (!resp.ok) {
    return NextResponse.json(
      { error: `upstream ${resp.status}` },
      { status: resp.status },
    );
  }

  // SHA-stamped assets are immutable → longer browser cache is safe.
  // Rolling assets stay short so a rollback becomes visible within 1h.
  const immutable = SNAPSHOT_RE.test(filePath) || Boolean(CDN_SHA);
  const cacheControl = immutable
    ? 'public, s-maxage=604800, stale-while-revalidate=86400, max-age=86400, immutable'
    : 'public, s-maxage=604800, stale-while-revalidate=86400, max-age=3600';

  return new NextResponse(resp.body, {
    status: 200,
    headers: {
      'Content-Type': contentType,
      'Cache-Control': cacheControl,
      'Access-Control-Allow-Origin': '*',
      ...(CDN_SHA ? { 'X-Weft-CDN-SHA': CDN_SHA } : {}),
    },
  });
}
