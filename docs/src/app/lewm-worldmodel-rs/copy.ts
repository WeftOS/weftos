/* All prose for /lewm-worldmodel-rs.
 * Sourced from .planning/symposiums/lewm-worldmodel/{synthesis.md, observer-lattice-design.md, diagram.md}
 * and docs/adr/adr-048-*.md through adr-058-*.md.
 */

export const HERO = {
  eyebrow: 'v0.7.0 · feature/lewm-worldmodel · 11 ADRs',
  title: 'LeWM under the DAG',
  subtitle: 'JEPA as a sub-layer of CMVG',
  meta: 'ADR-048 → ADR-058 · SIGReg isotropic-Gaussian manifold',
  scrollHint: 'scroll to descend',
} as const;

export const TLDR = {
  prequote:
    'The right move is not to replace ECC with LeWM — it is to put LeWM under ECC as a subsymbolic sensor substrate.',
  attribution: '— synthesis.md, Round 1',
  body: 'Two timescales, one loop: the causal graph learns what the right variables are (slow, symbolic); the predictor learns how they evolve (fast, differentiable). The sensor network is primary and self-sufficient. The world model is an additive consumer.',
} as const;

export const INVERSION = {
  eyebrow: 'ADR-058 · The Constitutional Invariant',
  title: 'We inverted it.',
  before: {
    label: 'First-revision framing',
    body: 'A lattice fed by observers. Sensors point inward at a shared world-model. Coupling everywhere.',
  },
  after: {
    label: 'Decoupling invariant',
    body: 'A self-sufficient sensor pipeline with the world model as one optional subscriber. Data flows one-way.',
  },
  rules: [
    'Senders are observational-only. No gradient semantics on the wire.',
    'World-model disappearance does not degrade sensor-network functionality.',
    'Sensor disappearance degrades the world model gracefully, not catastrophically.',
    '`LatticeApi` callers must handle `ServiceUnavailable`.',
    'Smaller agents have local-only fallback paths.',
  ],
} as const;

export const PANELS = [
  {
    id: 'sensor-plane',
    number: '❶',
    title: 'Sensor Plane',
    tagline: 'per node · always-on',
    adrs: ['ADR-056', 'ADR-057'],
    body: [
      'RGB-D · IMU · Proprio · LiDAR · Audio stream into a three-step learned pipeline:',
      'Collect (transmit-gate) → Aggregate (multi-sensor fusion) → Encode (SIGReg-manifold ViT-tiny).',
      'Three trainable RVF-hosted small models per sensor class. Hot-swap at tick alignment. Auto-rollback on sigreg_health < 0.85 for 30 s.',
    ],
    accent: 'cyan' as const,
  },
  {
    id: 'observation-wire',
    number: '❷',
    title: 'Observation Wire',
    tagline: 'mesh.sensor.v1.* · CBOR + Ed25519',
    adrs: ['ADR-053', 'ADR-022'],
    body: [
      'Three topics under mesh.sensor.v1.*: encoded (primary, 10 Hz, burstable 100 Hz), consensus (conditional, world-model output), control (drift / surprise / plans).',
      'Packets are observational-only. No gradient proxies. No consumer-specific framing.',
      'Every frame Ed25519-signed, ExoChain-indexed. Replay is the single source of truth.',
    ],
    accent: 'cyan' as const,
  },
  {
    id: 'local-consumers',
    number: '❸',
    title: 'Local Consumers',
    tagline: 'authoritative · per node',
    adrs: ['ADR-048', 'ADR-046'],
    body: [
      'hnsw_eml (ruvector) · ECC Causal Graph (causal.rs, 3417 LOC) · CMVG forest with StructureTag::LatentWorldModel.',
      'LatentHandle(Uuid) lives in CausalNode.metadata — no schema break.',
      'DEMOCRITUS runs the 1 kHz servo off an ArcSwap<ActionPlan>. The 1 ms cognitive tick never runs the planner inline.',
    ],
    accent: 'mint' as const,
  },
  {
    id: 'worldmodel-service',
    number: '❹',
    title: 'WorldModelService',
    tagline: 'cluster · optional · additive',
    adrs: ['ADR-052', 'ADR-054'],
    body: [
      'Fusion Transformer → Predictor pred_φ → LatentPlanner (CEM default · MPPI warm-start · gradient-shooting).',
      'Exposes LatticeApi as a first-class ServiceApi. observe · predict · plan · recall · subscribe_surprise · subscribe_drift.',
      'Three deployment topologies: single primary · Raft-elected primary + standby · peer-to-peer.',
      'ServiceUnavailable is a contract value. Callers fall back to local-ECC-only paths.',
    ],
    accent: 'violet' as const,
  },
  {
    id: 'training-layer',
    number: '❺',
    title: 'Training Layer',
    tagline: 'two surfaces · decoupled cadences',
    adrs: ['ADR-055', 'ADR-057'],
    body: [
      'Edge intelligence: offline, per-sensor-class. Weights bundled as RVF segments. Hot-swap safe because the SIGReg manifold is contract, not implementation.',
      'World-model streaming merge: online, in-service. Importance-weighted replay per sensor class — rare-event sensors retain long residency.',
      'Four-condition AND rollback: SIGReg health · probing accuracy · VoE surprise differentiation · temporal-straightening score.',
    ],
    accent: 'violet' as const,
  },
  {
    id: 'exochain',
    number: '❻',
    title: 'ExoChain',
    tagline: 'attestation & replay spine',
    adrs: ['ADR-022', 'ADR-033'],
    body: [
      'Every sensor frame · every checkpoint · every RVF segment activation · every rollback · every governance topology change · every LatticeApi capability grant.',
      'Standby and peer WorldModelServices catch up from replay. Training data comes from ExoChain, never from the live bus.',
    ],
    accent: 'amber' as const,
  },
] as const;

export const HOEA = {
  eyebrow: 'Diagram ②',
  title: 'The cycle the world-model trains through',
  body: 'Three timescales run concurrently: 1 ms DEMOCRITUS servo tick · 10 Hz planner + predictor + HNSW insert · continuous streaming-merge checkpoints. Scroll to set the cycle speed.',
  states: [
    {
      label: 'HYPOTHESIZE',
      detail: 'Causal graph proposes do(a_t) intervention',
    },
    {
      label: 'OBSERVE',
      detail: 'Predictor pred_φ runs · new z_{t+1} arrives on wire',
    },
    {
      label: 'EVALUATE',
      detail: 'VoE surprise · Δλ_Fiedler shift · SIGReg health update',
    },
    {
      label: 'ADJUST',
      detail: 'CausalCollapseModel · edge-weight regression · predictor online step',
    },
  ],
} as const;

export const ADRS = [
  { n: '048', title: 'JEPA as CMVG sub-layer', cite: 'Causal graph stays authoritative · latent world model publishes impulses' },
  { n: '049', title: 'Adopt candle, write the model', cite: 'Framework: candle · Model: ours · EmlBackend trait for swap' },
  { n: '050', title: 'SIGReg + isotropic-Gaussian latent contract', cite: 'Versioned · ExoChain-attested · degrade not crash' },
  { n: '051', title: 'LatentPlanner trait', cite: 'CEM default · MPPI feature-gated · per-node advertised' },
  { n: '052', title: 'LatticeApi as ServiceApi', cite: 'observe · predict · plan · recall · surprise · drift' },
  { n: '053', title: 'Observation wire protocol', cite: 'mesh.sensor.v1.* · CBOR · Ed25519 · observational-only' },
  { n: '054', title: 'WorldModelService consumer split', cite: 'Single · hot-standby · peer-to-peer · ServiceUnavailable' },
  { n: '055', title: 'Training-packet consumer contract', cite: 'Streaming merge · importance-weighted replay · 4-condition AND rollback' },
  { n: '056', title: 'Pipeline Service first-class', cite: 'In-process · separate-process · delegated-key attestation' },
  { n: '057', title: 'Edge intelligence contract', cite: 'RVF-hosted · hot-swap at tick · 30 s SIGReg rollback' },
  { n: '058', title: 'Sensor ↔ world-model decoupling invariant', cite: 'Sensor network primary · world model additive · the constitution', hero: true },
] as const;

export const SENSORS = [
  {
    id: 'rgbd',
    idx: '01',
    title: 'RGB-D camera',
    tagline: 'depth + color stream — primary spatial sensor for grasp planning, free-space, and object identity.',
    codec: 'depth16 · YUV420 → patch tokens',
    specs: [
      { label: 'rate', value: '30 fps · 320 × 180' },
      { label: 'collect', value: 'novelty gate · MAD on depth Δ' },
      { label: 'aggregate', value: 'patch tokens · 16×9 grid' },
      { label: 'encode', value: 'ViT-tiny · 6L · 192d' },
      { label: 'fallback', value: 'graceful · proprio + lidar fill' },
    ],
    tags: ['ADR-056', 'ADR-057'],
  },
  {
    id: 'imu',
    idx: '02',
    title: 'IMU · 6-axis',
    tagline: 'fast inertial sensor — drives the 1 kHz servo loop and bootstraps every prediction.',
    codec: 'float32 · 6-axis → 1d temporal tokens',
    specs: [
      { label: 'rate', value: '1 kHz · 6-axis (acc + gyro)' },
      { label: 'collect', value: 'ring buffer 100 ms · raw' },
      { label: 'aggregate', value: 'temporal align with vision' },
      { label: 'encode', value: 'temporal-CNN → 192d' },
      { label: 'fallback', value: 'never drops · servo critical' },
    ],
    tags: ['ADR-047', 'ADR-056'],
  },
  {
    id: 'proprio',
    idx: '03',
    title: 'Proprioception',
    tagline: '12 joint angles + torques — DEMOCRITUS reads here every millisecond.',
    codec: 'float32 · 24-channel → joint tokens',
    specs: [
      { label: 'rate', value: '1 kHz · 12 joints (θ + τ)' },
      { label: 'collect', value: 'always-on · authoritative' },
      { label: 'aggregate', value: 'kinematic chain projection' },
      { label: 'encode', value: 'GNN over chain → 192d' },
      { label: 'fallback', value: 'local-only fallback path' },
    ],
    tags: ['ADR-047', 'ADR-053'],
  },
  {
    id: 'lidar',
    idx: '04',
    title: 'LiDAR · 96-beam',
    tagline: 'spatial truth — long-range geometry that anchors the visual world model.',
    codec: 'compressed pointcloud → voxel tokens',
    specs: [
      { label: 'rate', value: '20 Hz · 96 beams · 360°' },
      { label: 'collect', value: 'voxel hash · 10 cm³' },
      { label: 'aggregate', value: 'sparse-conv · radial bins' },
      { label: 'encode', value: 'PointTransformer-S → 192d' },
      { label: 'fallback', value: 'depth-only mode degraded' },
    ],
    tags: ['ADR-056', 'ADR-058'],
  },
  {
    id: 'audio',
    idx: '05',
    title: 'Audio · 48-mel',
    tagline: 'event sensor — surprise spikes, voice commands, environment classification.',
    codec: '16-bit PCM → log-mel spectrogram',
    specs: [
      { label: 'rate', value: '16 kHz · 48-mel · 25 ms hop' },
      { label: 'collect', value: 'VAD gate · energy threshold' },
      { label: 'aggregate', value: 'mel windows · 1 s context' },
      { label: 'encode', value: 'audio-ViT-nano → 192d' },
      { label: 'fallback', value: 'optional · world-model only' },
    ],
    tags: ['ADR-053', 'ADR-057'],
  },
] as const;

export const DEEPDIVE = {
  title: 'Inside one sensor pipeline.',
  subtitle:
    'Three small models per sensor class. Each is RVF-hosted, separately versioned, separately attested, and hot-swappable at tick alignment with 30-second SIGReg rollback.',
  collect:
    'A trained transmit-gate decides what is worth sending. MAD-novelty + saturation gate cut bandwidth ~7× without losing rare-event sensitivity.',
  aggregate:
    'Per-modality tokenizers feed a temporal-aligned cross-modal block. Sensor-drop is a contract, not an error: missing modalities degrade quality smoothly.',
  encode:
    'A 6-layer ViT-tiny projects to 192d under SIGReg + VICReg. The output is contract-checked against an isotropic Gaussian so any consumer can subscribe uniformly.',
  caption: 'Three RVF segments · three independent rollback domains · one shared latent contract.',
} as const;

export const TOPOLOGIES = [
  {
    id: 'single',
    title: 'Single primary',
    fleet: '1–10 nodes',
    body:
      'One WorldModelService for the entire fleet. Local ECC consumers fall back cleanly when it is unreachable. Simplest to operate, the right default for small teams.',
    props: [
      'lowest operational overhead',
      'ServiceUnavailable on outage',
      'best for early deployments',
    ],
  },
  {
    id: 'standby',
    title: 'Raft-elected primary + standby',
    fleet: '10–500 nodes',
    body:
      'Active primary with a hot standby trained off the same ExoChain replay. Failover is a leader election, not a re-train. Standby promotes within seconds.',
    props: [
      'subsecond failover',
      'standby trains off replay',
      'one authoritative writer',
    ],
  },
  {
    id: 'p2p',
    title: 'Peer-to-peer mesh',
    fleet: '500+ nodes · low-trust',
    body:
      'Every node runs its own WorldModelService and gossips checkpoints over mesh. No central authority. Used when fleets cross trust boundaries or partition tolerance dominates.',
    props: [
      'no central writer',
      'gossip-checkpointed',
      'partition-tolerant by construction',
    ],
  },
] as const;

export const CLOSER = {
  quote:
    'The sensor network is a self-sufficient three-step learned pipeline whose emissions live on a SIGReg manifold so any consumer, including an optional world model, can subscribe uniformly.',
  kicker: 'ADR-058 — the constitutional invariant — is what makes every other decision on this page consistent.',
  ctas: [
    { label: 'Read the synthesis', href: 'https://github.com/WeftOS/weftos/tree/feature/lewm-worldmodel/.planning/symposiums/lewm-worldmodel' },
    { label: 'Inspect the ADRs', href: 'https://github.com/WeftOS/weftos/tree/feature/lewm-worldmodel/docs/adr' },
    { label: 'Browse the workspace', href: 'https://github.com/WeftOS/weftos' },
  ],
} as const;
