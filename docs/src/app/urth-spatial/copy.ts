/* Prose for /urth-spatial. Sourced from docs/research/spatial-intelligence-2026/ (2026-09-21). */

export const HERO = {
  eyebrow: 'Urth · spatial indexes · 2026-09-21',
  title: 'The world is not a video.',
  subtitle:
    'WeftOS already indexes where, similar, and why. Urth densifies the planet only where we observe.',
  meta: 'BVH · HNSW · Graph Views · ADR-056 / 078 / 079',
  scrollHint: 'scroll — renderer, simulator, planner',
};

export const THESIS =
  'In 2026 “world model” names three jobs. WeftOS keeps them apart: appearance may be generated; metric shape lives in the BVH; plans ride Graph Views and optional LeWM. Generative fill is never treated as surveyed truth.';

export const FUNCTIONS = [
  {
    id: 'renderer',
    kicker: 'Renderer',
    title: 'How it looks',
    body: 'Gaussian splats, Marble exports, Genie frames, SOG. Pretty, fast, often fake beyond the capture frustum.',
    weftos: 'splat.sog / appearance train (ADR-078)',
    accent: 'amber' as const,
  },
  {
    id: 'simulator',
    kicker: 'Simulator',
    title: 'What has shape',
    body: 'AABBs, collider meshes, Object vs Event leaves. This is the only column Urth treats as metric.',
    weftos: 'clawft-bvh + weftos-leaf-types (ADR-056)',
    accent: 'mint' as const,
  },
  {
    id: 'planner',
    kicker: 'Planner',
    title: 'What to do next',
    body: 'Agents, fusion graphs, latent predictors. Useful priors — never the world source of truth.',
    weftos: 'Graph Views F1–F10 · LeWM optional (ADR-090)',
    accent: 'violet' as const,
  },
];

export const INDEXES = [
  {
    id: 'bvh',
    title: 'BVH',
    answers: 'where / when / shape',
    status: 'Shipped A–F',
    detail:
      'Median-split AABB tree. Point, sphere, ray, frustum, spatial kNN. Optional VectorRef on payloads. Kernel SpatialService behind ecc. Live daemon RPC is a residual.',
  },
  {
    id: 'hnsw',
    title: 'HNSW',
    answers: 'looks like / means like',
    status: 'Live primary ANN',
    detail:
      'Feature similarity. DiskANN is a deferred cold tier (slow serial Vamana build; hybrid metric bug). Join is composition, not a second tree inside the BVH.',
  },
  {
    id: 'causal',
    title: 'Causal + UNID',
    answers: 'why / who cites whom',
    status: 'ECC substrate',
    detail:
      'Ordered walks and direct lookup. Not geometry. Panopticon (ADR-069) would reverse-resolve chain_seq across all lenses — still proposed.',
  },
  {
    id: 'views',
    title: 'Graph Views',
    answers: 'fusion, then promote',
    status: 'Research ops',
    detail:
      'Purpose-scoped live graphs. Bind BVH as hard spatial source (F2), fuse, promote stable components to Object leaves (F9). Not a second world.',
  },
];

export const SHIPPED = [
  'WEFT-716–720 Phases A–E (tree, tags, service, COW, CLI)',
  'WEFT-721–723 VectorRef + Phase F join helpers',
  'WEFT-708/709 W0 scene stub + W1 geometric partition (export)',
  'WEFT-713 Urth E0 doctrine + world-builder expert',
];

export const RESIDUALS = [
  {
    title: 'Live BVH publish',
    why: 'W1 still writes bvh_published: false. Structure extract does not enter the index.',
  },
  {
    title: 'Daemon spatial RPC',
    why: 'spatial_cli_e2e is ignored. Reattach via SpatialService.',
  },
  {
    title: 'F9 promote',
    why: 'Graph Views research exists; stable components do not yet mint Object leaves.',
  },
  {
    title: 'Urth E1 / E2',
    why: 'Region hierarchy and OSM basemap ingest were unchecked on E0.',
  },
  {
    title: 'Panopticon',
    why: 'ADR-069 reverse join by chain_seq is still proposed.',
  },
  {
    title: 'DiskANN cold tier',
    why: 'Deferred, not disqualified. HNSW stays primary.',
  },
];

export const LANDSCAPE = [
  {
    id: 'S1',
    title: 'Marble + Atlas',
    pri: 'Apply language',
    blurb: 'World Labs dual export (splats + collider mesh) is the commercial form of ADR-078. Atlas (2026-09-01) is an omni camera-controlled model. Spark 2.0 streams 3DGS with LOD.',
    href: '/docs/weftos/research/spatial-intelligence',
  },
  {
    id: 'S2',
    title: 'Genie 3 · Cosmos · WBench',
    pri: 'Compose as priors',
    blurb: 'Interactive video worlds. Minutes of consistency, not a surveyed planet. May feed LeWM rollouts. Not Urth geometry.',
    href: '/docs/weftos/research/spatial-intelligence',
  },
  {
    id: 'S3',
    title: 'VGGT · MASt3R · SpatialLM',
    pri: 'Apply pipeline',
    blurb: 'Feed-forward reconstruction can sit before overnight COLMAP. SpatialLM proposes indoor layout from noisy RGB video — a W1 layout proposer with vector: none.',
    href: '/docs/weftos/research/spatial-intelligence',
  },
  {
    id: 'S4',
    title: 'HOV-SG · ConceptGraphs',
    pri: 'Apply hierarchy',
    blurb: 'Floor → room → object graphs. Embeddings stay on VectorRef / HNSW. Geometry stays in the BVH. Promote, do not flatten.',
    href: '/docs/weftos/research/spatial-intelligence',
  },
  {
    id: 'S5',
    title: 'CityGaussian · Octree-GS',
    pri: 'Apply appearance LOD',
    blurb: 'Large-scale splat LOD is the appearance twin of Urth L2–L4 and region-sharded BVH. One planetary Gaussian train is forbidden.',
    href: '/docs/weftos/research/spatial-intelligence',
  },
  {
    id: 'S6',
    title: 'V-JEPA 2.1',
    pri: 'Watch / visual index',
    blurb: 'Latent predictive WM. Confirms ADR-090: optional sub-layer. Dense features may become a visual HNSW namespace.',
    href: '/docs/weftos/research/spatial-intelligence',
  },
  {
    id: 'S7',
    title: 'WorldGraph · SuperSplat',
    pri: 'Compose',
    blurb: 'rUv typed twin + provenance cards over a splat. Geometry SoT remains BVH, not CSI occupancy.',
    href: '/docs/weftos/research/spatial-intelligence',
  },
  {
    id: 'S8',
    title: 'Open video WMs',
    pri: 'Watch',
    blurb: 'Astronex-World, Matrix-Game 3.0, HY-World, Yume. Do not pick a champion this cycle.',
    href: '/docs/weftos/research/spatial-intelligence',
  },
];

export const QUEUE_NEW = [
  'Optional VGGT / MASt3R-SLAM path before COLMAP',
  'SpatialLM (or equal) as W1 layout proposer',
  'Octree-GS / CityGaussian appearance LOD keyed to region ids',
  'Scene-graph hierarchy as Graph View materialization',
  'Marble dual-export language in product copy (already our split)',
  'V-JEPA 2.1 dense features as visual index_id — never SoT',
];

export const CLOSER = {
  title: 'Sparse first. Then densify.',
  body: 'The spatial-index decision is closed. The work left is publish, promote, and ingest — plus a 2026 reconstruction stack that can feed the same leaves.',
  links: [
    { label: 'Survey index', href: 'https://github.com/WeftOS/weftos/blob/0.8-metaharness/docs/research/spatial-intelligence-2026/README.md' },
    { label: 'Urth ADR-079', href: '/docs/weftos/vision/urth' },
    { label: 'LeWM scrolly', href: '/lewm-worldmodel-rs' },
    { label: 'Graph Views', href: 'https://github.com/WeftOS/weftos/blob/0.8-metaharness/docs/research/graph-views.md' },
  ],
};
