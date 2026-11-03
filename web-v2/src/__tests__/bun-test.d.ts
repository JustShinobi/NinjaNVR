declare module "bun:test" {
  export function describe(name: string, fn: () => void): void;
  export function test(name: string, fn: () => void | Promise<void>): void;
  export interface Matchers<T> {
    toBe(expected: T): void;
    toEqual(expected: unknown): void;
    toContain(expected: string): void;
    toBeTruthy(): void;
    toBeFalsy(): void;
    not: Matchers<T>;
  }
  export function expect<T>(actual: T): Matchers<T>;
}
