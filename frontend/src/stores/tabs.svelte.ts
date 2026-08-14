// Open entity tabs in the sidebar. Subscription lifecycles are owned
// by the components rendered inside a tab (Opened subscribes on mount,
// unsubscribes on destroy), so closing a tab needs no store-side
// cleanup beyond dropping it from the list.

let openIds = $state<string[]>([]);
let activeId = $state<string | null>(null);

export function openTabs(): string[] {
  return openIds;
}

export function activeTab(): string | null {
  return activeId;
}

export function openTab(id: string): void {
  if (!openIds.includes(id)) openIds = [...openIds, id];
  activeId = id;
}

export function focusTab(id: string): void {
  if (openIds.includes(id)) activeId = id;
}

export function closeTab(id: string): void {
  openIds = openIds.filter((candidate) => candidate !== id);
  if (activeId === id) activeId = openIds.at(-1) ?? null;
}
