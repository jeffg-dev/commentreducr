export function area(w: number, h: number): number {
  // Multiply width by height.
  return w * h;
}

export function label(name: string): string {
  // Prefix the name with a tag.
  return "tag:" + name;
}
