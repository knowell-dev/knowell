import { ApiError } from './api';

/** Reactive async data holder: loading / error / data, with reload and stale-response guard. */
export class Resource<T> {
  data = $state<T | undefined>(undefined);
  error = $state<ApiError | undefined>(undefined);
  loading = $state(true);
  private seq = 0;
  private readonly fetcher: () => Promise<T>;

  constructor(fetcher: () => Promise<T>) {
    this.fetcher = fetcher;
  }

  async load(): Promise<void> {
    const mine = ++this.seq;
    this.loading = true;
    this.error = undefined;
    try {
      const v = await this.fetcher();
      if (mine !== this.seq) return;
      this.data = v;
    } catch (e) {
      if (mine !== this.seq) return;
      this.error =
        e instanceof ApiError ? e : new ApiError('unknown', 'unexpected error in the panel');
    } finally {
      if (mine === this.seq) this.loading = false;
    }
  }

  /** Keeps current data on screen while refreshing. */
  reload = (): Promise<void> => this.load();
}

/** Create inside a component: loads once on mount. */
export function useResource<T>(fetcher: () => Promise<T>): Resource<T> {
  const r = new Resource(fetcher);
  void r.load();
  return r;
}
