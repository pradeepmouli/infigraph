export const T = { 'tsx-key': { z: 1 } } as const satisfies Record<string, unknown>;
export const U = ({ 'tsx-two': 2 } satisfies Record<string, number>) as const;
