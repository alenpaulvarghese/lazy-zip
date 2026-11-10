<script lang="ts">
  import { ask, open, save } from "@tauri-apps/plugin-dialog";
  import { tick } from "svelte";
  import { findExistingFiles, scanZip } from "$lib/api";
  import DownloadsPanel from "$lib/components/DownloadsPanel.svelte";
  import HeadersPanel, { type HeaderRow } from "$lib/components/HeadersPanel.svelte";
  import HelpBar from "$lib/components/HelpBar.svelte";
  import Spinner from "$lib/components/Spinner.svelte";
  import TreeView from "$lib/components/TreeView.svelte";
  import { DownloadManager } from "$lib/downloads.svelte";
  import { calcEfficiency, formatBytes, formatCount, MAX_WORKERS, MIN_WORKERS, normalizeWorkers, plural } from "$lib/format";
  import {
    partiallySelected,
    resolveSelection,
    selectRange,
    toggleSelected,
    type Selection,
  } from "$lib/selection";
  import {
    allFolderIds,
    ancestorsOf,
    buildIndex,
    createMatcher,
    searchRows,
    TreeSorter,
    visibleRows,
    type SortKey,
    type TreeIndex,
  } from "$lib/tree";
  import { headersToSend, sameHeaders, withLoginHint, type Header } from "$lib/headers";
  import type { ShortcutContext } from "$lib/shortcuts";
  import type { ScanResult } from "$lib/types";

  const SEARCH_DEBOUNCE_MS = 100;

  let url = $state("");
  let scannedUrl = "";
  let scanResult = $state.raw<ScanResult | null>(null);
  let index = $state.raw<TreeIndex | null>(null);
  let isLoading = $state(false);
  let errorMsg = $state("");

  // Headers can hold passwords and tokens, so they're only ever kept in memory.
  let headerRows = $state<HeaderRow[]>([]);
  let scannedHeaders = $state.raw<Header[]>([]);
  let showHeaders = $state(false);
  let headersPanel = $state<ReturnType<typeof HeadersPanel>>();
  let pendingHeaders = $derived(headersToSend(headerRows));
  let headerCount = $derived(headerRows.filter((row) => row.name.trim() || row.value.trim()).length);
  let headersStale = $derived(
    scanResult !== null && !("headers" in pendingHeaders && sameHeaders(pendingHeaders.headers, scannedHeaders)),
  );

  // Tree state
  let expanded = $state.raw<ReadonlySet<number>>(new Set());
  let selection = $state.raw<Selection>(new Set());
  let focusId = $state<number | null>(null);
  let rangeAnchor: number | null = null;

  // Search and sort
  let searchInput = $state("");
  let searchQuery = $state("");
  let searchTimer: ReturnType<typeof setTimeout> | null = null;
  let sortKey = $state<SortKey>("name");
  let sortDescending = $state(false);
  let searchField = $state<HTMLInputElement>();

  let matcher = $derived(createMatcher(searchQuery));
  let order = $derived({ key: sortKey, descending: sortDescending });
  let sorter = $derived(index ? new TreeSorter(index, order) : null);
  let rows = $derived.by(() => {
    if (!index || !sorter) return [];
    return matcher ? searchRows(index, matcher, order) : visibleRows(sorter, expanded);
  });
  let partial = $derived(index ? partiallySelected(index, selection) : new Set<number>());
  let resolved = $derived(index && selection.size > 0 ? resolveSelection(index, selection) : null);
  let breadcrumb = $derived(
    index && focusId !== null ? [...ancestorsOf(index, focusId), focusId] : [],
  );

  // Help bar
  let shortcutContext = $state<ShortcutContext>("url");
  let showShortcuts = $state(false);

  // Downloads
  const downloads = new DownloadManager();

  // Settings
  let downloadWorkers = $state(10);
  let showSettings = $state(false);

  // Success toast
  let toastMsg = $state("");
  let toastTimer: ReturnType<typeof setTimeout> | null = $state(null);

  function showToast(msg: string) {
    if (toastTimer) clearTimeout(toastTimer);
    toastMsg = msg;
    toastTimer = setTimeout(() => { toastMsg = ""; toastTimer = null; }, 4000);
  }

  async function scan(event: Event) {
    event.preventDefault();
    if (!url) return;
    if ("error" in pendingHeaders) {
      errorMsg = pendingHeaders.error;
      showHeaders = true;
      return;
    }
    const scanningHeaders = pendingHeaders.headers;

    isLoading = true;
    errorMsg = "";
    scanResult = null;
    index = null;
    expanded = new Set();
    selection = new Set();
    focusId = null;
    rangeAnchor = null;
    clearSearch();

    const scanningUrl = url;
    try {
      const result = await scanZip(scanningUrl, scanningHeaders);
      const newIndex = buildIndex(result.nodes);
      scannedUrl = scanningUrl;
      scannedHeaders = scanningHeaders;
      index = newIndex;
      scanResult = result;
      // A single top-level folder would otherwise be one lonely row.
      if (newIndex.roots.length === 1 && result.nodes[newIndex.roots[0]].is_dir) {
        expanded = new Set(newIndex.roots);
      }
    } catch (err: unknown) {
      errorMsg = withLoginHint(String(err), scanningHeaders.length > 0);
    } finally {
      isLoading = false;
    }
  }

  async function toggleHeaders() {
    showHeaders = !showHeaders;
    if (showHeaders && headerRows.length === 0) {
      await tick();
      await headersPanel?.addRow();
    }
  }

  function setSearch(value: string) {
    searchInput = value;
    if (searchTimer) clearTimeout(searchTimer);
    searchTimer = setTimeout(() => {
      searchQuery = value;
      searchTimer = null;
    }, SEARCH_DEBOUNCE_MS);
  }

  function clearSearch() {
    if (searchTimer) clearTimeout(searchTimer);
    searchTimer = null;
    searchInput = "";
    searchQuery = "";
  }

  function toggleExpanded(id: number) {
    const next = new Set(expanded);
    if (!next.delete(id)) next.add(id);
    expanded = next;
  }

  /** Shows a node in the full tree, leaving search if needed. */
  function reveal(id: number) {
    if (!index) return;
    clearSearch();
    expanded = new Set([...expanded, ...ancestorsOf(index, id)]);
    focusId = id;
  }

  function activate(id: number) {
    if (!index) return;
    if (matcher) {
      reveal(id);
    } else if (index.nodes[id].is_dir) {
      toggleExpanded(id);
    } else {
      void downloadNode(id);
    }
  }

  function handleToggleSelected(id: number) {
    if (!index) return;
    selection = toggleSelected(index, selection, id);
    rangeAnchor = id;
  }

  function handleSelectRange(id: number) {
    selection = selectRange(selection, rows, rangeAnchor ?? id, id);
    rangeAnchor ??= id;
  }

  async function downloadNode(id: number) {
    if (!index) return;
    const node = index.nodes[id];
    if (node.is_dir) {
      await downloadSelection(new Set([id]));
      return;
    }

    const dest = await save({ defaultPath: node.name, title: "Save file as..." });
    if (!dest) return;
    await downloads.start({
      label: node.name,
      url: scannedUrl,
      headers: scannedHeaders,
      target: { kind: "file", zipPath: node.path, dest },
      sizes: new Map([[node.path, node.size]]),
      workers: normalizeWorkers(downloadWorkers),
    });
  }

  async function downloadSelection(items: Selection) {
    if (!index) return;
    const { base, files, sizes, roots } = resolveSelection(index, items);
    if (files.length === 0) {
      showToast("Nothing to download: the selection has no files");
      return;
    }

    const destDir = await open({ directory: true, title: "Select destination folder" });
    if (!destDir) return;

    let filesToDownload = files;
    const existing = await findExistingFiles(destDir, base, files);
    if (existing.length > 0) {
      const redownload = await ask(
        `${formatCount(existing.length)} of ${plural(files.length, "file")} already exist in the destination. Do you want to download them again?`,
        { title: "Files Already Exist", kind: "warning", okLabel: "Download All Again", cancelLabel: "Skip Existing" },
      );
      if (!redownload) {
        const existingSet = new Set(existing);
        filesToDownload = files.filter((path) => !existingSet.has(path));
        for (const path of existing) sizes.delete(path);
        if (filesToDownload.length === 0) {
          showToast("All selected files already exist");
          return;
        }
      }
    }

    const label =
      roots.length === 1
        ? `${index.nodes[roots[0]].name}${index.nodes[roots[0]].is_dir ? "/" : ""}`
        : plural(roots.length, "item");
    await downloads.start({
      label,
      url: scannedUrl,
      headers: scannedHeaders,
      target: { kind: "selection", destDir, base, files: filesToDownload },
      sizes,
      workers: normalizeWorkers(downloadWorkers),
    });
  }

  function handleGlobalKeydown(event: KeyboardEvent) {
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "f" && scanResult) {
      event.preventDefault();
      searchField?.focus();
      searchField?.select();
    } else if (event.key === "?" && !isTextField(event.target)) {
      event.preventDefault();
      showShortcuts = !showShortcuts;
    }
  }

  // Runs before other handlers, so Escape closes the list without also clearing the selection.
  function handleEscapeCapture(event: KeyboardEvent) {
    if (event.key === "Escape" && showShortcuts) {
      event.preventDefault();
      event.stopPropagation();
      showShortcuts = false;
    }
  }

  function isTextField(target: EventTarget | null): boolean {
    return target instanceof HTMLInputElement && target.type !== "checkbox";
  }

  // Other controls keep the last context, so the hints don't flicker while using a button.
  function handleFocusIn(event: FocusEvent) {
    const target = event.target;
    if (!(target instanceof HTMLElement)) return;
    if (target.matches('input[type="url"], input[data-header-field]')) shortcutContext = "url";
    else if (target.matches('input[type="search"]')) shortcutContext = "search";
    else if (target.matches('[role="tree"]')) shortcutContext = "tree";
  }
</script>

<svelte:window onkeydown={handleGlobalKeydown} onkeydowncapture={handleEscapeCapture} onfocusin={handleFocusIn} />

<div class="h-screen flex flex-col bg-bg-base font-sans">
  <!-- Minimal top bar -->
  <header
    class="flex items-center h-11 px-4 bg-bg-surface border-b border-border-default shrink-0 select-none"
  >
    <div class="flex items-center gap-2">
      <svg class="w-4 h-4 text-accent" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
        <path d="M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4" />
        <polyline points="7 10 12 15 17 10" />
        <line x1="12" y1="15" x2="12" y2="3" />
      </svg>
      <span class="text-sm font-semibold text-text-primary tracking-tight">Lazy ZIP</span>
    </div>

    <!-- Settings toggle -->
    <div class="ml-auto flex items-center gap-2 relative">
      <button
        class="p-1.5 rounded-md hover:bg-bg-hover/60 transition-colors text-text-tertiary hover:text-text-secondary"
        aria-label="Settings"
        onclick={() => (showSettings = !showSettings)}
      >
        <svg class="w-4 h-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <circle cx="12" cy="12" r="3" />
          <path d="M19.4 15a1.65 1.65 0 00.33 1.82l.06.06a2 2 0 010 2.83 2 2 0 01-2.83 0l-.06-.06a1.65 1.65 0 00-1.82-.33 1.65 1.65 0 00-1 1.51V21a2 2 0 01-4 0v-.09A1.65 1.65 0 009 19.4a1.65 1.65 0 00-1.82.33l-.06.06a2 2 0 01-2.83-2.83l.06-.06A1.65 1.65 0 004.68 15a1.65 1.65 0 00-1.51-1H3a2 2 0 010-4h.09A1.65 1.65 0 004.6 9a1.65 1.65 0 00-.33-1.82l-.06-.06a2 2 0 012.83-2.83l.06.06A1.65 1.65 0 009 4.68a1.65 1.65 0 001-1.51V3a2 2 0 014 0v.09a1.65 1.65 0 001 1.51 1.65 1.65 0 001.82-.33l.06-.06a2 2 0 012.83 2.83l-.06.06A1.65 1.65 0 0019.4 9a1.65 1.65 0 001.51 1H21a2 2 0 010 4h-.09a1.65 1.65 0 00-1.51 1z" />
        </svg>
      </button>

      {#if showSettings}
        <div class="absolute right-0 top-full mt-1 bg-bg-elevated border border-border-default rounded-lg shadow-lg p-3 z-50 w-52">
          <label class="flex items-center justify-between text-xs text-text-secondary">
            <span>Download Workers</span>
            <input
              type="number"
              min={MIN_WORKERS}
              max={MAX_WORKERS}
              bind:value={downloadWorkers}
              class="w-14 bg-bg-input text-text-primary text-xs px-2 py-1 rounded border border-border-default text-center outline-none focus:border-accent"
            />
          </label>
        </div>
      {/if}
    </div>
  </header>

  <!-- Main content -->
  <main class="flex-1 flex flex-col overflow-hidden">
    <!-- URL input area -->
    <div class="px-4 py-3 bg-bg-surface border-b border-border-default shrink-0">
      <form onsubmit={scan}>
        <div class="flex gap-2 items-center">
          <div class="flex-1 relative">
            <input
              type="url"
              bind:value={url}
              placeholder="Paste a remote .zip URL..."
              class="w-full bg-bg-input text-text-primary text-sm px-3 py-2 rounded-lg border border-border-default focus:border-accent focus:ring-1 focus:ring-accent/30 outline-none transition-all placeholder:text-text-tertiary"
              required
            />
          </div>
          <button
            type="button"
            class="text-sm px-3 py-2 rounded-lg border transition-colors flex items-center gap-1.5 shrink-0 {showHeaders || headerCount > 0
              ? 'border-accent/40 text-text-primary bg-accent-subtle'
              : 'border-border-default text-text-secondary hover:text-text-primary hover:bg-bg-hover/60'}"
            aria-expanded={showHeaders}
            aria-controls="request-headers"
            title="Add headers for archives that need a login or a token"
            onclick={toggleHeaders}
          >
            <svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true">
              <path d="M21 2l-2 2m-7.61 7.61a5.5 5.5 0 11-7.78 7.78 5.5 5.5 0 017.78-7.78zm0 0L15.5 7.5m0 0l3 3L22 7l-3-3m-3.5 3.5L19 4" />
            </svg>
            Headers
            {#if headerCount > 0}
              <span class="min-w-[18px] h-[18px] px-1 rounded-full bg-accent text-bg-base text-[10px] font-semibold leading-[18px] text-center">{headerCount}</span>
            {/if}
          </button>
          <button
            type="submit"
            disabled={isLoading}
            class="bg-accent hover:bg-accent-hover text-bg-base text-sm font-semibold px-5 py-2 rounded-lg transition-all disabled:opacity-40 disabled:cursor-not-allowed flex items-center gap-2 shrink-0"
          >
            {#if isLoading}
              <Spinner />
              Scanning…
            {:else}
              Scan
            {/if}
          </button>
        </div>
        {#if showHeaders}
          <HeadersPanel bind:this={headersPanel} bind:rows={headerRows} stale={headersStale} />
        {/if}
      </form>

      {#if errorMsg}
        <div class="mt-2 px-3 py-2 bg-error-bg text-error text-xs rounded-lg border border-error/20 flex items-center justify-between">
          <span>{errorMsg}</span>
          <button class="ml-2 text-error/60 hover:text-error" onclick={() => (errorMsg = "")}>✕</button>
        </div>
      {/if}
    </div>

    {#if scanResult}
      <!-- Stats row -->
      <div class="flex gap-3 px-4 py-3 bg-bg-surface border-b border-border-default shrink-0">
        <div class="flex-1 bg-bg-card rounded-lg px-3 py-2 border border-border-default">
          <div class="text-[10px] uppercase tracking-widest text-text-tertiary font-medium">Total Size</div>
          <div class="text-sm font-semibold text-text-primary mt-0.5">{formatBytes(scanResult.total_size)}</div>
        </div>
        <div class="flex-1 bg-bg-card rounded-lg px-3 py-2 border border-border-default">
          <div class="text-[10px] uppercase tracking-widest text-text-tertiary font-medium">Fetched</div>
          <div class="text-sm font-semibold text-text-primary mt-0.5">{formatBytes(scanResult.fetched_size)}</div>
        </div>
        <div class="flex-1 bg-bg-card rounded-lg px-3 py-2 border border-border-default">
          <div class="text-[10px] uppercase tracking-widest text-text-tertiary font-medium">Efficiency</div>
          <div class="text-sm font-semibold text-success mt-0.5">{calcEfficiency(scanResult.total_size, scanResult.fetched_size)}% saved</div>
        </div>
      </div>

      {#if scanResult.rejected_count > 0}
        <details class="px-4 py-2 bg-error-bg border-b border-border-default shrink-0 text-xs">
          <summary class="cursor-pointer text-error">
            {plural(scanResult.rejected_count, "entry", "entries")} can't be downloaded because the name is unsafe
          </summary>
          <ul class="mt-1.5 max-h-32 overflow-auto space-y-1 list-none p-0 m-0 select-text">
            {#each scanResult.rejected as entry, i (i)}
              <li class="text-text-secondary break-all"><span class="text-text-primary">{entry.name}</span>: {entry.reason}</li>
            {/each}
            {#if scanResult.rejected_count > scanResult.rejected.length}
              <li class="text-text-tertiary">and {formatCount(scanResult.rejected_count - scanResult.rejected.length)} more</li>
            {/if}
          </ul>
        </details>
      {/if}

      <!-- Search and sort -->
      <div class="flex flex-wrap gap-2 items-center px-4 py-2 border-b border-border-default shrink-0">
        <div class="relative flex-1 min-w-48">
          <input
            bind:this={searchField}
            type="search"
            value={searchInput}
            oninput={(event) => setSearch(event.currentTarget.value)}
            onkeydown={(event) => { if (event.key === "Escape") clearSearch(); }}
            placeholder="Search files, or use a pattern like *.json"
            aria-label="Search files"
            class="w-full bg-bg-input text-text-primary text-xs px-3 py-1.5 rounded-md border border-border-default focus:border-accent outline-none placeholder:text-text-tertiary"
          />
        </div>
        <label class="flex items-center gap-1.5 text-xs text-text-secondary">
          Sort
          <select bind:value={sortKey} class="bg-bg-input text-text-primary text-xs px-2 py-1.5 rounded-md border border-border-default outline-none focus:border-accent">
            <option value="name">Name</option>
            <option value="size">Size</option>
            <option value="compressed">Compressed size</option>
            <option value="ratio">Compression ratio</option>
          </select>
        </label>
        <button
          type="button"
          class="text-xs text-text-secondary hover:text-text-primary px-2 py-1.5 rounded-md border border-border-default"
          aria-label={sortDescending ? "Sorted descending" : "Sorted ascending"}
          onclick={() => (sortDescending = !sortDescending)}
        >{sortDescending ? "↓" : "↑"}</button>
        {#if matcher}
          <span class="text-xs text-text-secondary" role="status">{plural(rows.length, "match", "matches")}</span>
          {#if rows.length > 0}
            <button type="button" class="text-xs text-accent hover:text-accent-hover" onclick={() => (selection = new Set([...selection, ...rows]))}>Select all</button>
          {/if}
        {:else}
          <button type="button" class="text-xs text-text-secondary hover:text-text-primary" onclick={() => { if (index) expanded = new Set(allFolderIds(index)); }}>Expand all</button>
          <button type="button" class="text-xs text-text-secondary hover:text-text-primary" onclick={() => (expanded = new Set())}>Collapse all</button>
        {/if}
      </div>

      {#if resolved}
        <!-- Selection actions -->
        <div class="flex items-center gap-3 px-4 py-2 bg-accent-subtle border-b border-border-default shrink-0 text-xs">
          <span class="text-text-primary font-medium">{plural(resolved.files.length, "file")} selected</span>
          <span class="text-text-secondary">{formatBytes(resolved.bytes)}</span>
          <button type="button" class="ml-auto text-text-secondary hover:text-text-primary" onclick={() => (selection = new Set())}>Clear</button>
          <button type="button" class="bg-accent hover:bg-accent-hover text-bg-base font-semibold px-3 py-1 rounded-md" onclick={() => downloadSelection(selection)}>Download</button>
        </div>
      {/if}

      {#if breadcrumb.length > 1}
        <nav class="flex items-center gap-1 px-4 pt-1.5 text-[11px] text-text-tertiary shrink-0 overflow-hidden whitespace-nowrap" aria-label="Location of the focused item">
          {#each breadcrumb as id, i (id)}
            {#if i > 0}<span aria-hidden="true">›</span>{/if}
            <button type="button" class="hover:text-text-secondary truncate" onclick={() => reveal(id)}>{index?.nodes[id].name}</button>
          {/each}
        </nav>
      {/if}

      <!-- File tree -->
      {#if index && sorter && index.nodes.length > 0}
        {#if matcher && rows.length === 0}
          <p class="flex-1 text-text-tertiary text-sm italic px-6 py-4">No files match this search.</p>
        {:else}
          <TreeView
            {index}
            {sorter}
            {rows}
            {matcher}
            {expanded}
            {selection}
            {partial}
            {focusId}
            onFocus={(id) => (focusId = id)}
            onToggleExpanded={toggleExpanded}
            onToggleSelected={handleToggleSelected}
            onSelectRange={handleSelectRange}
            onSelectAll={() => (selection = new Set([...selection, ...rows]))}
            onClearSelection={() => (selection = new Set())}
            onActivate={activate}
            onDownload={downloadNode}
          />
        {/if}
      {:else}
        <p class="flex-1 text-text-tertiary text-sm italic px-6 py-4">No downloadable files found in this archive.</p>
      {/if}
    {:else if !isLoading}
      <!-- Empty state -->
      <div class="flex-1 flex flex-col items-center justify-center gap-3 opacity-40">
        <svg class="w-12 h-12 text-text-tertiary" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1">
          <path d="M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4" />
          <polyline points="7 10 12 15 17 10" />
          <line x1="12" y1="15" x2="12" y2="3" />
        </svg>
        <p class="text-sm text-text-tertiary">Enter a remote ZIP URL to explore its contents</p>
      </div>
    {/if}
  </main>

  <DownloadsPanel manager={downloads} />

  <HelpBar
    bind:open={showShortcuts}
    state={{
      context: scanResult ? shortcutContext : "url",
      hasArchive: scanResult !== null,
      searching: matcher !== null,
      selectionCount: selection.size,
      focusedIsDir: index && focusId !== null ? index.nodes[focusId].is_dir : null,
    }}
  />

  <!-- Success toast -->
  {#if toastMsg}
    <div class="absolute bottom-10 left-1/2 -translate-x-1/2 bg-bg-elevated border border-success/30 rounded-lg shadow-lg px-4 py-2.5 flex items-center gap-2 z-50 animate-fade-in">
      <svg class="w-4 h-4 text-success shrink-0" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
        <path d="M20 6L9 17l-5-5" />
      </svg>
      <span class="text-sm text-text-primary">{toastMsg}</span>
      <button class="ml-2 text-text-tertiary hover:text-text-secondary text-xs" onclick={() => { toastMsg = ""; if (toastTimer) clearTimeout(toastTimer); }}>✕</button>
    </div>
  {/if}
</div>
