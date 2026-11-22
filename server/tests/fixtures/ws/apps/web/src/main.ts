import { button } from "@acme/ui";
export const html = button({ label: "x", size: "xl" });
function f(a: { x: { y: { z: number } } }) { return a; }
f({ x: { y: { z: "s" } } });
class A { private p = 1 }
class B { private p = 2 }
const a: A = new B();
