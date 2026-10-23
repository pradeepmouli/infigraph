export interface EmitConfig {
  strict?: boolean;
  generatedIdTables: string;
  nested: { inner: number; optInner?: string };
}
interface SerializedBase { factoryShape: string; unresolved?: boolean }
export type Lit = { k: string; m?: number };
export namespace NS { export interface InNs { p: number } }
export class Holder { prop = 1; typed: { a: number } = { a: 1 }; }
export const table: { rows: number } = { rows: 1 };
export function f(opts: { skipped: number }): { alsoSkipped: number } {
  return { alsoSkipped: opts.skipped };
}
