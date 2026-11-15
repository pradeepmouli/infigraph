// Method signatures: interface and type-literal members, abstract methods, and
// the overload shapes around them. See tests/ts_method_signatures.rs.

export interface ConcreteSeam {
  sightings(pair: number): readonly string[];
  count: number;
  'quoted-name'(x: string): void;
  optional?(y: number): void;
  over(a: string): void;
  over(a: number): void;
  tail: string;
  [key: string]: unknown;
  (call: number): void;
  new (c: number): ConcreteSeam;
}

// Two declarations of one interface, each with an `m`.
export interface Merged {
  m(a: string): void;
}
export interface Merged {
  m(a: number): void;
  other(): void;
}

export type Lit = { run(a: number): void; flag: boolean };

export type Nested = { inner: { go(x: number): void } };

export abstract class Base {
  abstract area(): number;
  abstract name(x: string): string;
  static make(): Base {
    return null as unknown as Base;
  }
}

// Implements what Base declares: a different owner, so no collision.
export class Impl extends Base {
  area(): number {
    return 1;
  }
  name(x: string): string {
    return x;
  }
}

// Overload signatures above an implementation stay folded into it.
export class Over {
  foo(a: string): void;
  foo(a: number): void;
  foo(a: unknown): void {}
}

// A parameter's inline type is no symbol.
export function takes(p: { inner(x: number): void }) {}
