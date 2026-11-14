// Quoted keys: properties of an interface, of a nested type literal, and of a class.
export interface Flags {
  readonly 'c-strings': true;
}

export interface Cfg {
  'a-b': {
    'c-d': string;
  };
}

export class Settings {
  'x-y' = 1;
}
