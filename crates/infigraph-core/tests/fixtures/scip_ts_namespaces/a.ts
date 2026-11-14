// Namespaces that only exist as namespaces, nested like sittir's `Declaration::Interface`.
export namespace Clause {
  export interface Shape {
    'bound-clauses': boolean;
  }
}

export namespace Declaration {
  export namespace Interface {
    export interface Props {
      readonly 'bound-clauses': true;
    }
  }
}

export namespace Element {
  export const tag = 'x';
}

// A namespace merged with a class of the same name: one symbol, the class's.
export class Box {
  size = 1;
}
export namespace Box {
  export const depth = 2;
}

// A namespace merged with an interface of the same name: one symbol, the interface's.
export interface Opts {
  verbose: boolean;
}
namespace Opts {
  export type Mode = 'a' | 'b';
}

// Several blocks of one namespace: one symbol, at the first block.
export namespace Repeated {
  export const first = 1;
}
export namespace Repeated {
  export const second = 2;
}

// An ambient namespace, and a dotted one.
declare namespace Ambient {
  export const ready: boolean;
}
export namespace Dotted.Inner {
  export const deep = 3;
}

// A module declared by a string is not a namespace of this file.
declare module 'external-lib' {
  export const shape: number;
}
