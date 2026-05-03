/**
 * Remark plugin: wraps the value half of "**Label:** Value" list items
 * on resource reference pages with a span carrying a category class so
 * CSS can color-code them.
 *
 * Triggered on any `<ul>` whose items all start with **bold-text:** and
 * sit directly under an `<h3>` (i.e. the attribute meta-info pattern).
 */
import { visit } from 'unist-util-visit';

const TYPE_CATEGORIES = {
  bool: ['Bool', 'Boolean'],
  number: ['Int', 'Integer', 'Long', 'Double', 'Float', 'Number'],
  string: ['String', 'Ipv4Cidr', 'Ipv6Cidr', 'CIDR', 'ARN', 'Url', 'Uri'],
  enum: ['Enum'],
  list: ['List', 'Array'],
  map: ['Map', 'Object', 'Struct'],
};

function nodeText(node) {
  if (node == null) return '';
  if (typeof node.value === 'string') return node.value;
  if (Array.isArray(node.children)) return node.children.map(nodeText).join('');
  return '';
}

function categorize(value) {
  const v = value.trim();
  for (const [cat, kws] of Object.entries(TYPE_CATEGORIES)) {
    if (kws.some((k) => v.startsWith(k) || v.includes(`(${k})`))) return cat;
  }
  return null;
}

export default function remarkTypePills() {
  return (tree) => {
    visit(tree, 'list', (node) => {
      if (!node.children?.length) return;

      // Detect "meta list" — every item starts with strong "Label:" text
      const allMeta = node.children.every((li) => {
        const para = li.children?.[0];
        if (para?.type !== 'paragraph') return false;
        const first = para.children?.[0];
        return first?.type === 'strong';
      });
      if (!allMeta) return;

      // Tag the list and its items so CSS can hook in
      node.data = node.data || {};
      node.data.hProperties = { ...(node.data.hProperties || {}), className: ['attr-meta'] };

      for (const li of node.children) {
        const para = li.children[0];
        if (!para?.children) continue;

        const labelNode = para.children[0]; // strong
        const labelText = (labelNode.children?.[0]?.value || '').replace(/:\s*$/, '').trim();
        const labelKey = labelText.toLowerCase().replace(/\s+/g, '-');

        // Combine the trailing text/inline nodes into the value text
        const valueNodes = para.children.slice(1);
        const valueText = valueNodes.map(nodeText).join('').trim();

        // Decide a category class
        let cat = null;
        if (labelKey === 'type') cat = categorize(valueText);
        else if (labelKey === 'required') cat = valueText.toLowerCase() === 'yes' ? 'req-yes' : 'req-no';
        else if (labelKey === 'create-only') cat = valueText.toLowerCase() === 'yes' ? 'replace' : null;

        // Tag the <li> with classes
        li.data = li.data || {};
        const classes = [`meta-${labelKey}`];
        if (cat) classes.push(`meta-${cat}`);
        li.data.hProperties = { ...(li.data.hProperties || {}), className: classes };

        // Wrap the value half in a <span class="meta-value"> so CSS can
        // color it without coloring the label.
        if (valueNodes.length) {
          para.children = [
            labelNode,
            {
              type: 'html',
              value: '<span class="meta-value">',
            },
            ...valueNodes,
            {
              type: 'html',
              value: '</span>',
            },
          ];
        }
      }
    });
  };
}
