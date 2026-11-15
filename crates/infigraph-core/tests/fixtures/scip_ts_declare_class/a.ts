// Class bodies with signatures and no implementation: `declare class`, and an
// ambient class. A method_signature is folded into an implementation of the
// same name when the body has one, and is a symbol of its own otherwise.

export declare class X {
  foo(): void;
  bar(a: string): void;
  bar(a: number): void;
  static make(): X;
  'quoted-name'(x: string): void;
}

export declare abstract class Y {
  abstract area(): number;
  describe(): string;
}

// Overloads above an implementation stay folded; a signature of another name
// in the same body has no implementation and is a symbol.
export declare class Mixed {
  only(): void;
}

export class Fold {
  fold(a: string): void;
  fold(a: number): void;
  fold(a: unknown): void {}
}

// The same shape with a name another class also uses: SCIP's definitions of the
// overload set then find no lone candidate by name, which is the import's
// behaviour for a method name shared in one file, with or without signatures.
export class A1 {
  go(a: string): void;
  go(a: number): void;
  go(a: unknown): void {}
}
export class A2 {
  go(): void {}
}
