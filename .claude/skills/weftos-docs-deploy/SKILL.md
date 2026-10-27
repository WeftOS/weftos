---
name: weftos-docs-deploy
description: Deploy Fumadocs site to Vercel at weftos.weavelogic.ai (always run after docs/src changes)
triggers:
  - deploy docs
  - update docs site
  - docs changed
  - redeploy vercel
  - weftos docs
---

# WeftOS Docs Deploy Skill

Deploy the Fumadocs documentation site to Vercel whenever `docs/src/` content changes.

## When to Run

**Always after any changes to `docs/src/`** — new pages, updated content, API docs regeneration, or config changes. The Vercel deploy is NOT automatic until GitHub integration is configured with root dir = `docs/src`.

## Quick Deploy

```bash
source .env
cd docs/src
npx vercel deploy --prod --yes --token "$VERCEL_TOKEN"
```

## Full Deploy with API Docs Refresh

```bash
# 1. Regenerate rustdoc API reference (if Rust code changed)
scripts/generate-api-docs.sh

# 2. Deploy to Vercel
source .env
cd docs/src
npx vercel deploy --prod --yes --token "$VERCEL_TOKEN"
```

## Verify

```bash
curl -s -o /dev/null -w "%{http_code}" https://weftos.weavelogic.ai/
curl -s -o /dev/null -w "%{http_code}" https://weftos.weavelogic.ai/api/weftos/index.html
```

## Configuration

- **Vercel project**: `src` (should be renamed to `weftos-docs` in dashboard)
- **Custom domain**: `weftos.weavelogic.ai` (CNAME → `cname.vercel-dns.com`)
- **Root directory**: `docs/src` (set in Vercel dashboard for auto-deploy)
- **Token**: `VERCEL_TOKEN` in `.env`
- **Account**: `mathew-9074` / `mathew-9074s-projects` scope

## Auto-Deploy Setup (one-time)

To enable automatic deploys on push to master:
1. Go to Vercel dashboard → project `src`
2. Settings → Git → Root Directory → set to `docs/src`
3. Connected repo: `WeftOS/weftos`
4. After this, every push to master auto-deploys

## What Gets Deployed

- Fumadocs MDX pages (65+ pages across WeftOS, clawft, Getting Started, Guides, Reference, Vision)
- Rustdoc API reference at `/api/` (1,014 HTML pages, pre-generated)
- Landing page with install CTA and package links
- Next.js 16 with Tailwind CSS 4

## Troubleshooting

| Problem | Fix |
|---------|-----|
| 404 on weftos.weavelogic.ai | DNS: CNAME `weftos` → `cname.vercel-dns.com` |
| Old content after push | Run manual deploy (auto-deploy needs root dir config) |
| API docs missing | Run `scripts/generate-api-docs.sh` first |
| Build fails | Check `cd docs/src && npm run build` locally |
