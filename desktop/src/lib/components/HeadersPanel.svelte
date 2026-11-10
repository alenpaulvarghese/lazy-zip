<script lang="ts" module>
  export interface HeaderRow {
    id: number;
    name: string;
    value: string;
    visible: boolean;
  }
</script>

<script lang="ts">
  import { tick } from "svelte";
  import { splitHeaderLine } from "$lib/headers";

  let { rows = $bindable(), stale }: { rows: HeaderRow[]; stale: boolean } = $props();

  let panel = $state<HTMLElement>();
  let nextId = 0;

  function focusField(id: number, field: "name" | "add") {
    const selector = field === "add" ? "[data-add-header]" : `[data-header-name="${id}"]`;
    panel?.querySelector<HTMLElement>(selector)?.focus();
  }

  export async function addRow() {
    const id = nextId++;
    rows.push({ id, name: "", value: "", visible: false });
    await tick();
    focusField(id, "name");
  }

  async function removeRow(index: number) {
    rows.splice(index, 1);
    await tick();
    const next = rows[Math.min(index, rows.length - 1)];
    focusField(next?.id ?? -1, next ? "name" : "add");
  }

  // Pasting a whole line like "Authorization: Bearer abc" fills in both fields.
  function handleNamePaste(event: ClipboardEvent, row: HeaderRow) {
    if (row.name || row.value) return;
    const header = splitHeaderLine(event.clipboardData?.getData("text") ?? "");
    if (!header) return;
    event.preventDefault();
    row.name = header.name;
    row.value = header.value;
  }

  const fieldClass =
    "min-w-0 bg-bg-input text-text-primary text-xs px-2 py-1.5 rounded-md border border-border-default focus:border-accent outline-none placeholder:text-text-tertiary";
</script>

<div bind:this={panel} id="request-headers" class="mt-2 space-y-1.5" role="group" aria-label="Request headers">
  {#each rows as row, i (row.id)}
    <div class="flex items-center gap-1.5">
      <input
        type="text"
        bind:value={row.name}
        onpaste={(event) => handleNamePaste(event, row)}
        placeholder="Name, e.g. Authorization"
        aria-label="Header {i + 1} name"
        autocomplete="off"
        spellcheck="false"
        data-header-field
        data-header-name={row.id}
        class="{fieldClass} w-48 shrink-0"
      />
      <input
        type={row.visible ? "text" : "password"}
        bind:value={row.value}
        placeholder="Value"
        aria-label="Header {i + 1} value"
        autocomplete="off"
        spellcheck="false"
        data-header-field
        class="{fieldClass} flex-1"
      />
      <button
        type="button"
        class="p-1.5 rounded-md text-text-tertiary hover:text-text-secondary hover:bg-bg-hover/60"
        aria-label={row.visible ? `Hide header ${i + 1} value` : `Show header ${i + 1} value`}
        aria-pressed={row.visible}
        title={row.visible ? "Hide value" : "Show value"}
        onclick={() => (row.visible = !row.visible)}
      >
        <svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true">
          <path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z" />
          <circle cx="12" cy="12" r="3" />
          {#if row.visible}<line x1="3" y1="3" x2="21" y2="21" />{/if}
        </svg>
      </button>
      <button
        type="button"
        class="p-1.5 rounded-md text-text-tertiary hover:text-error hover:bg-bg-hover/60 text-xs leading-none"
        aria-label="Remove header {i + 1}"
        title="Remove header"
        onclick={() => removeRow(i)}
      >✕</button>
    </div>
  {/each}

  <div class="flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px]">
    <button type="button" data-add-header class="text-accent hover:text-accent-hover" onclick={addRow}>+ Add header</button>
    <span class="text-text-tertiary">
      For logins and tokens. Kept in memory until you quit, never saved, and not sent to other sites when the server redirects.
    </span>
    {#if stale}
      <span class="text-text-secondary" role="status">Scan again to use the changed headers.</span>
    {/if}
  </div>
</div>
