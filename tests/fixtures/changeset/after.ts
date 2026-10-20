export function area(w: number, h: number): number {
  // Multiply width by height.
  return w * h * 2;
}

export function label(name: string): string {
  // Prefix the name with a tag.
  return "tag:" + name;
}

export function half(x: number): number {
  // Divide by two.
  return x / 2;
}
