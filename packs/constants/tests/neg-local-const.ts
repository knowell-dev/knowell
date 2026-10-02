export function build(id: string) {
  const local = "not.module.level";
  let mutable = "also.not";
  return `${local}${mutable}${id}`;
}
