import { mount } from 'svelte';
import './app.css';
import App from './App.svelte';
import { createApiClient } from '$lib/api';
import { applyTheme, loadTheme } from '$lib/theme';

applyTheme(loadTheme());

const target = document.getElementById('app');
if (!target) throw new Error('missing #app element');

const { client, mock } = await createApiClient();
mount(App, { target, props: { client, mock } });
