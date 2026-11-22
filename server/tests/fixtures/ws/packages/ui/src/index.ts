export interface ButtonProps { label: string; size: "sm" | "lg" }
export function button(p: ButtonProps): string { return p.label + p.size; }
export const uiBroken: number = "ui";
