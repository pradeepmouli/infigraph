// Not valid TypeScript, but the parser accepts it: an abstract signature and a
// concrete method of one name in one class body.
export abstract class Weird {
  abstract w(): void;
  w() {}
}
