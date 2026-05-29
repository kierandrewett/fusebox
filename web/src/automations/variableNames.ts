/** Variable names available for autocomplete: the automation's persisted
 *  runtime variables plus any names referenced by variable blocks currently in
 *  the editor graph, so a variable can be suggested the moment it's used. */
export function mergeVariableNames(
  persisted: string[] | null | undefined,
  fromGraph: string[] | null | undefined,
): string[] {
  const set = new Set<string>();
  for (const v of persisted ?? []) set.add(v);
  for (const v of fromGraph ?? []) set.add(v);
  return Array.from(set).sort();
}
