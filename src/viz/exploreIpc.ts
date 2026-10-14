/**
 * Explore IPC layer (spec §7/§8): typed wrappers for the DOM-mode
 * datasets (`get_folder_view`, `node_details`, `top_sizes`, `age_map`,
 * `list_children`, `get_breadcrumb`) and the shell actions
 * (`open_node`, `reveal_in_explorer`, `copy_path`, `preview_text`).
 * Every call is generation-tagged; stale results throw and the callers
 * drop them (spec §9 IPC rule).
 */
import { invoke } from "../lib/ipc";
import { userFacingError } from "../lib/userFacingError";

export interface CategoryDot {
  color: number;
  label: string;
  size: number;
}

export interface FolderCardData {
  id: number;
  name: string;
  size: number;
  itemCount: number;
  fileCount: number;
  folderCount: number;
  categories: CategoryDot[];
  modified: number;
  protected: boolean;
}

export interface FileTileData {
  id: number;
  name: string;
  size: number;
  category: string;
  categoryColor: number;
  modified: number;
  protected: boolean;
  cloud: boolean;
}

export interface FolderViewData {
  generation: number;
  node: number;
  name: string;
  size: number;
  fileCount: number;
  folderCount: number;
  folders: FolderCardData[];
  files: FileTileData[];
  filesCapped: boolean;
}

export interface LargestItemData {
  id: number;
  name: string;
  size: number;
  isDir: boolean;
}

export interface NodeDetailsData {
  id: number;
  name: string;
  isDir: boolean;
  kind: string;
  kindColor: number;
  path: string;
  size: number;
  shareOfScan: number;
  logical: number;
  overhead: number;
  savings: number;
  files: number;
  folders: number;
  ofParent: number;
  modified: number;
  created: number;
  isCloud: boolean;
  isProtected: boolean;
  largest: LargestItemData[];
}

export type TopScopeId = "in-folder" | "files-anywhere" | "folders-anywhere";

export interface TopRowData {
  rank: number;
  id: number;
  name: string;
  parentPath: string;
  size: number;
  kind: string;
  isDir: boolean;
  share: number;
  color: number;
}

export interface TopSizesData {
  generation: number;
  node: number;
  scope: string;
  rows: TopRowData[];
  total: number;
}

export interface BigRowData {
  id: number;
  name: string;
  path: string;
  logical: number;
  onDisk: number;
  modified: number;
  protected: boolean;
}

export interface MonthHeatmapData {
  firstYear: number;
  lastYear: number;
  /** bytes[(year - firstYear) * 12 + month] */
  bytes: number[];
  max: number;
  /** `[year, zero-based month]`, matching the `bytes` column index. */
  busiest: [number, number] | null;
}

export interface AgeMapDataData {
  generation: number;
  node: number;
  buckets: number[];
  bucketLabels: string[];
  total: number;
  heatmap: MonthHeatmapData;
  big: BigRowData[];
}

export interface ListRowData {
  id: number;
  name: string;
  isDir: boolean;
  hasChildren: boolean;
  share: number;
  size: number;
  /** Direct children count (folders; files report 0) — the Items column. */
  items: number;
  category: string;
  color: number;
  protected: boolean;
  cloud: boolean;
  modified: number;
}

export interface CrumbData {
  id: number;
  name: string;
  size: number;
}

export interface TextPreviewData {
  text: string;
  truncated: boolean;
  read: number;
}

/** Stale-generation marker (the drop rule's contract). */
export function isStaleError(e: unknown): boolean {
  return String(e).includes("stale generation");
}

export async function getFolderView(
  generation: number,
  node: number,
  filter: string,
): Promise<FolderViewData> {
  return invoke<FolderViewData>("get_folder_view", {
    generation,
    node,
    filter: filter || null,
  });
}

export async function getNodeDetails(
  generation: number,
  id: number,
): Promise<NodeDetailsData> {
  return invoke<NodeDetailsData>("node_details", { generation, id });
}

export async function getTopSizes(
  generation: number,
  node: number,
  scope: TopScopeId,
  filter: string,
): Promise<TopSizesData> {
  return invoke<TopSizesData>("top_sizes", {
    generation,
    node,
    scope,
    filter: filter || null,
  });
}

export async function getAgeMap(generation: number, node: number): Promise<AgeMapDataData> {
  return invoke<AgeMapDataData>("age_map", { generation, node });
}

export async function getListChildren(
  generation: number,
  node: number,
  filter: string,
): Promise<ListRowData[]> {
  return invoke<ListRowData[]>("list_children", {
    generation,
    node,
    filter: filter || null,
  });
}

export async function getBreadcrumb(generation: number, node: number): Promise<CrumbData[]> {
  return invoke<CrumbData[]>("get_breadcrumb", { generation, node });
}

export async function openNode(generation: number, id: number): Promise<void> {
  try {
    await invoke("open_node", { generation, id });
  } catch (e) {
    // A failed launch must NEVER be silent: "clicked and nothing
    // happened" is the exact defect that killed the Focus button
    // (session 14). The Rust layer returns a readable reason (no file
    // association, access denied); surface it on the toast bus.
    window.dispatchEvent(
      new CustomEvent("db-toast", {
        detail: { text: `Couldn't open this item — ${userFacingError(e)}` },
      }),
    );
  }
}

export async function revealInExplorer(generation: number, id: number): Promise<void> {
  await invoke("reveal_in_explorer", { generation, id });
}

export async function copyPath(generation: number, id: number): Promise<void> {
  await invoke("copy_path", { generation, id });
}

export async function previewText(generation: number, id: number): Promise<TextPreviewData> {
  return invoke<TextPreviewData>("preview_text", { generation, id });
}
