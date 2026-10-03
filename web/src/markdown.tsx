import type { ComponentChildren, JSX } from "preact";
import MarkdownIt, { type Token } from "markdown-it";
import { PlainDocument } from "./components/DocumentBody";

const parser = new MarkdownIt({ html: false, linkify: false });
const tags: Record<string, string> = {
  paragraph_open: "p", heading_open: "heading", blockquote_open: "blockquote",
  bullet_list_open: "ul", ordered_list_open: "ol", list_item_open: "li",
  em_open: "em", strong_open: "strong", s_open: "s", link_open: "a",
  table_open: "table", thead_open: "thead", tbody_open: "tbody", tr_open: "tr", th_open: "th", td_open: "td",
};

export function allowedLink(href: string): boolean {
  if (/[\u0000-\u0020\u007f]/.test(href)) return false;
  if (/^mailto:/i.test(href)) return true;
  if (!/^https?:\/\/[^/\\?#]+/i.test(href)) return false;
  try { return Boolean(new URL(href).hostname); } catch { return false; }
}

function wrapToken(token: Token, content: ComponentChildren[]): ComponentChildren[] {
  const known = tags[token.type];
  if (!known) { content.unshift(token.content); return content; }
  let tag = known;
  const attrs: Record<string, string | number> = {};
  if (known === "heading") tag = /^h[1-6]$/.test(token.tag) ? token.tag : "p";
  if (tag === "a") {
    const href = String(token.attrGet("href") ?? "");
    if (!allowedLink(href)) return content;
    attrs.href = href; attrs.rel = "noopener noreferrer";
  }
  if (tag === "ol") {
    const start = Number(token.attrGet("start") ?? 1);
    if (Number.isSafeInteger(start)) attrs.start = start;
  }
  if (tag === "th" || tag === "td") {
    const align = String(token.attrGet("style") ?? "").match(/^text-align:(left|right|center)$/)?.[1];
    if (align) attrs.class = "align-" + align;
  }
  const Tag = tag as keyof JSX.IntrinsicElements;
  return [<Tag {...attrs}>{content}</Tag>];
}

// Explicit frames handle nesting without consuming the JavaScript call stack.
// Only known token types choose tags; no parser attributes are spread into the DOM.
export function tokenNodes(tokens: Token[]): ComponentChildren[] {
  type Frame = { tokens: Token[]; index: number; children: ComponentChildren[]; open?: Token };
  const stack: Frame[] = [{ tokens, index: 0, children: [] }];
  while (stack.length) {
    const frame = stack[stack.length - 1]!;
    const token = frame.tokens[frame.index++];
    if (!token || token.nesting === -1) {
      stack.pop();
      const children = frame.open ? wrapToken(frame.open, frame.children) : frame.children;
      if (!stack.length) return children;
      const parent = stack[stack.length - 1]!.children;
      for (const child of children) parent.push(child);
    } else if (token.nesting === 1) stack.push({ tokens: frame.tokens, index: frame.index, children: [], open: token });
    else if (token.type === "inline") stack.push({ tokens: token.children ?? [], index: 0, children: [] });
    else if (token.type === "code_inline") frame.children.push(<code>{token.content}</code>);
    else if (token.type === "fence" || token.type === "code_block") frame.children.push(<pre><code>{token.content}</code></pre>);
    else if (token.type === "hardbreak") frame.children.push(<br />);
    else if (token.type === "softbreak") frame.children.push("\n");
    else if (token.type === "hr") frame.children.push(<hr />);
    else if (token.type === "image") frame.children.push(token.content + " (" + (token.attrGet("src") ?? "") + ")");
    else frame.children.push(token.content);
    // Paired tokens share one token stream; the parent resumes after its child closes.
    if ((!token || token.nesting === -1) && stack.length && frame.open) stack[stack.length - 1]!.index = frame.index;
  }
  return [];
}

export function Markdown({ body }: { body: string }) {
  try { return <div class="markdown">{tokenNodes(parser.parse(body, {}))}</div>; }
  catch { return PlainDocument({ body }); }
}
