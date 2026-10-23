/** A user. */
export interface User {
  name: string;
  greet(): void;
}

export type Id = string;

export enum Color { Red, Green }

export class Account implements User {
  name = "a";
  /** Say hello. */
  greet(): void {
    helper(this.name);
  }
  static make(): Account {
    return new Account();
  }
}

export function helper(s: string): number {
  return s.length;
}

export const double = (n: number): number => n * 2;

const LIMIT = 10;

namespace Util {
  export function inner() {}
}
