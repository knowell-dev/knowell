import type { Component } from 'svelte';
import Overview from '../routes/Overview.svelte';
import Workspaces from '../routes/Workspaces.svelte';
import Projects from '../routes/Projects.svelte';
import Indexes from '../routes/Indexes.svelte';
import Jobs from '../routes/Jobs.svelte';
import Search from '../routes/Search.svelte';
import Graph from '../routes/Graph.svelte';
import Domains from '../routes/Domains.svelte';
import Memory from '../routes/Memory.svelte';
import Rules from '../routes/Rules.svelte';
import Models from '../routes/Models.svelte';
import Quality from '../routes/Quality.svelte';
import Agents from '../routes/Agents.svelte';
import Integrations from '../routes/Integrations.svelte';
import Admin from '../routes/Admin.svelte';
import NotFound from '../routes/NotFound.svelte';

export interface RouteDef {
  path: string;
  title: string;
  group: 'Engine' | 'Explore' | 'Knowledge' | 'Models' | 'Operate';
  component: Component;
}

export const routes: RouteDef[] = [
  { path: '/', title: 'Overview', group: 'Engine', component: Overview },
  { path: '/workspaces', title: 'Workspaces', group: 'Engine', component: Workspaces },
  { path: '/projects', title: 'Projects', group: 'Engine', component: Projects },
  { path: '/indexes', title: 'Indexes', group: 'Engine', component: Indexes },
  { path: '/jobs', title: 'Jobs', group: 'Engine', component: Jobs },
  { path: '/search', title: 'Search playground', group: 'Explore', component: Search },
  { path: '/graph', title: 'Code graph', group: 'Explore', component: Graph },
  { path: '/domains', title: 'Domains and glossary', group: 'Explore', component: Domains },
  { path: '/memory', title: 'Memory', group: 'Knowledge', component: Memory },
  { path: '/rules', title: 'Rules', group: 'Knowledge', component: Rules },
  { path: '/models', title: 'Model profiles', group: 'Models', component: Models },
  { path: '/quality', title: 'Quality', group: 'Models', component: Quality },
  { path: '/agents', title: 'Agents and usage', group: 'Operate', component: Agents },
  { path: '/integrations', title: 'Integrations', group: 'Operate', component: Integrations },
  { path: '/admin', title: 'Administration', group: 'Operate', component: Admin }
];

export const notFound = NotFound;

export function matchRoute(path: string): RouteDef | undefined {
  return routes.find((r) => r.path === path);
}
