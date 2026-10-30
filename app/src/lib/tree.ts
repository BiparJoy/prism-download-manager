// A file list (a torrent's, a model's, a ZIP's) → folder tree with aggregated sizes.

export interface TreeNode {
  name: string;
  path: string;
  size: number;
  /** File indexes contained in this node (one for files). */
  files: number[];
  children: TreeNode[];
}

/** The same order as `localeCompare`, much faster for long lists (an archive's thousands of files). */
const byName = new Intl.Collator().compare;

export function buildTree(files: { index: number; path: string; size: number }[]): TreeNode {
  const root: TreeNode = { name: "", path: "", size: 0, files: [], children: [] };
  // Children by name, so a folder of thousands of files isn't searched once per file.
  const named = new Map<TreeNode, Map<string, TreeNode>>();
  for (const f of files) {
    const parts = f.path.split(/[\\/]+/).filter(Boolean);
    let node = root;
    node.size += f.size;
    node.files.push(f.index);
    parts.forEach((part, i) => {
      let siblings = named.get(node);
      if (!siblings) named.set(node, (siblings = new Map()));
      let child = siblings.get(part);
      if (!child) {
        child = { name: part, path: parts.slice(0, i + 1).join("/"), size: 0, files: [], children: [] };
        node.children.push(child);
        siblings.set(part, child);
      }
      child.size += f.size;
      child.files.push(f.index);
      node = child;
    });
  }
  const sort = (n: TreeNode) => {
    n.children.sort((a, b) => Number(b.children.length > 0) - Number(a.children.length > 0) || byName(a.name, b.name));
    n.children.forEach(sort);
  };
  sort(root);
  return root;
}

/** "all" | "none" | "some" for a node given the selected file indexes. */
export function nodeState(node: TreeNode, selected: Set<number>): "all" | "none" | "some" {
  const n = node.files.filter((i) => selected.has(i)).length;
  return n === 0 ? "none" : n === node.files.length ? "all" : "some";
}
