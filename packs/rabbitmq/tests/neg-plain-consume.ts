export function queueJob(queue: string[], job: string) {
  queue.push(job);
  const consume = (q: string) => q;
  return consume("not.a.queue");
}
