<script lang="ts">
  import type { Job, JobState } from '$lib/api';
  import { relativeTime } from '$lib/format';
  import Badge from './Badge.svelte';
  import Meter from './Meter.svelte';

  let { jobs, progress = {} }: { jobs: Job[]; progress?: Record<string, number> } = $props();

  const jobTone = (s: JobState) =>
    s === 'succeeded'
      ? 'ok'
      : s === 'failed' || s === 'dead'
        ? 'danger'
        : s === 'running'
          ? 'accent'
          : 'neutral';
</script>

<div class="table-wrap">
  <table class="table">
    <thead
      ><tr
        ><th>Job</th><th>Kind</th><th>Workspace / project</th><th>State</th><th class="num"
          >Attempts</th
        ><th>Progress</th><th>Queued</th><th>Updated</th></tr
      ></thead
    >
    <tbody>
      {#each jobs as j (j.id)}
        <tr>
          <td class="mono">{j.id}</td>
          <td><code>{j.kind}</code></td>
          <td>
            {#if j.projectName}{j.workspaceName ? `${j.workspaceName} / ` : ''}{j.projectName}
            {:else if j.workspaceName}{j.workspaceName}
            {:else}<span class="faint" title="The job payload names no project">not named</span
              >{/if}
          </td>
          <td
            ><Badge tone={jobTone(j.state)}>{j.state}</Badge>{#if j.error}<div
                class="small danger-text"
              >
                {j.error}
              </div>{/if}</td
          >
          <td class="num">{j.attempts}/{j.maxAttempts}</td>
          <td
            >{#if j.state === 'running'}{@const pr =
                progress[j.id] ?? j.progress}{#if pr !== null && pr !== undefined}<Meter
                  value={pr}
                  label="{j.id} progress"
                />{:else}<span class="faint" title="The worker does not report progress"
                  >not reported yet</span
                >{/if}{/if}</td
          >
          <td>{relativeTime(j.enqueuedAt)}</td>
          <td>{relativeTime(j.updatedAt)}</td>
        </tr>
      {/each}
    </tbody>
  </table>
</div>

<style>
  .danger-text {
    color: var(--danger);
  }
</style>
