type SitePositions = { counts: Map<string, number>; holders: Map<string, number> };
interface Info { pass: string }

export const X = { 'a-b': 1, c: { d: 2 } } as const satisfies Record<string, unknown>;

export const Y = { 'e-f': 3 } as const satisfies Record<string, unknown> as const;

export const W = { 'g-h': { k: 5 } satisfies Record<string, number> } as const satisfies Record<string, unknown>;

export const NO_POSITIONS: SitePositions = { counts: new Map(), holders: new Map() };

export const PLAIN: Record<string, Info> = { 'plain-key': { pass: 'x' } };
