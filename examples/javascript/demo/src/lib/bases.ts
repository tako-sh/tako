export type BasePreset = {
  slug: string;
  name: string;
  world: string;
  summary: string;
  source: string;
};

export type BaseImageUrls = {
  source: string;
  card: string;
  hero: string;
  thumb: string;
};

export type PlanetBase = BasePreset & {
  image: BaseImageUrls;
};

export const BASE_PRESETS: BasePreset[] = [
  {
    slug: "caloris-relay",
    name: "Caloris Relay",
    world: "Mercury",
    summary: "Solar-side comms and cargo relay on a crater rim.",
    source: "/images/caloris-relay.jpg",
  },
  {
    slug: "valles-hub",
    name: "Valles Hub",
    world: "Mars",
    summary: "Canyon logistics hub for rover convoys and habitat resupply.",
    source: "/images/valles-hub.jpg",
  },
  {
    slug: "europa-dock",
    name: "Europa Dock",
    world: "Europa",
    summary: "Ice-field docking base with heated pads and sealed haulers.",
    source: "/images/europa-dock.jpg",
  },
  {
    slug: "titan-yard",
    name: "Titan Yard",
    world: "Titan",
    summary: "Cold-weather supply yard on the edge of the methane lakes.",
    source: "/images/titan-yard.jpg",
  },
  {
    slug: "venus-aerostat",
    name: "Venus Aerostat",
    world: "Venus",
    summary: "High-atmosphere platform floating above the cloud deck.",
    source: "/images/venus-aerostat.jpg",
  },
  {
    slug: "shackleton",
    name: "Shackleton",
    world: "Moon",
    summary: "Polar crater base for ice extraction and lunar cargo hops.",
    source: "/images/shackleton.jpg",
  },
];

export function resolveBasePreset(slug: string): BasePreset {
  const exact = BASE_PRESETS.find((base) => base.slug === slug);
  if (exact) return exact;
  return BASE_PRESETS[hashSlug(slug) % BASE_PRESETS.length] ?? BASE_PRESETS[0]!;
}

function hashSlug(slug: string): number {
  let hash = 0;
  for (const char of slug) {
    hash = (hash * 31 + char.charCodeAt(0)) >>> 0;
  }
  return hash;
}
