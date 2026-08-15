// Tab state for the main view. `null` is the transcript — always
// present, always first, uncloseable. Entity ids are closeable tabs.
// Subscription lifecycles are owned by the components rendered in a
// tab (Opened subscribes on mount, unsubscribes on destroy), so
// closing a tab needs no store-side cleanup beyond dropping it.

let openIds = $state<string[]>([]);
let activeId = $state<string | null>(null);

export function openTabs(): string[] {
  return openIds;
}

/** The active tab: an entity id, or `null` for the transcript. */
export function activeTab(): string | null {
  return activeId;
}

export function openTab(id: string): void {
  if (!openIds.includes(id)) openIds = [...openIds, id];
  activeId = id;
}

export function focusTab(id: string | null): void {
  if (id === null || openIds.includes(id)) activeId = id;
}

export function closeTab(id: string): void {
  openIds = openIds.filter((candidate) => candidate !== id);
  if (activeId === id) activeId = null;
}
