/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** `mock` forces the mock client, `http` forces the real one (dev default is mock). */
  readonly VITE_API?: 'mock' | 'http';
  /** `hub` makes the mock client report the hub role (shows the Admin screen). */
  readonly VITE_MOCK_ROLE?: 'hub' | 'standalone';
  /** `server` makes the mock send `null` wherever the real server cannot know a value yet. */
  readonly VITE_MOCK_SHAPE?: 'server' | 'rich';
  /** `1` makes the mock answer 401 until a token starting with `kn_` is submitted. */
  readonly VITE_MOCK_LOGIN?: '1';
  /** `unavailable` makes engine-delegated routes answer 503 `engine_unavailable`. */
  readonly VITE_MOCK_ENGINE?: 'unavailable';
}
interface ImportMeta {
  readonly env: ImportMetaEnv;
}
