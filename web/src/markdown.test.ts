import { describe, expect, it, vi } from "vitest";
import type { ComponentChildren, VNode } from "preact";
import MarkdownIt, { type Token } from "markdown-it";
import { Markdown, allowedLink, tokenNodes } from "./markdown";

const webURL = (scheme: string, path = "") => scheme + ":" + "/" + "/example.com/" + path;
function nodes(value: ComponentChildren): VNode[] {
  const result: VNode[] = []; const stack: ComponentChildren[] = [value];
  while (stack.length) {
    const item = stack.pop();
    if (Array.isArray(item)) { for (let i = item.length - 1; i >= 0; i--) stack.push(item[i]); }
    else if (item && typeof item === "object" && "type" in item) { result.push(item); stack.push((item.props as { children?: ComponentChildren }).children); }
  }
  return result;
}
function text(value: ComponentChildren): string {
  const parts: string[] = []; const stack: ComponentChildren[] = [value];
  while (stack.length) {
    const item = stack.pop();
    if (Array.isArray(item)) { for (let i = item.length - 1; i >= 0; i--) stack.push(item[i]); }
    else if (item && typeof item === "object" && "props" in item) stack.push((item.props as { children?: ComponentChildren }).children);
    else if (item != null && typeof item !== "boolean") parts.push(String(item));
  }
  return parts.join("");
}
const tree = (body: string) => Markdown({ body });

describe("Markdown token renderer", () => {
  it("builds all allowed constructs as elements, retaining text and list start", () => {
    const body = ["# One", "## Two", "### Three", "#### Four", "##### Five", "###### Six", "", "Paragraph *em* **strong** ~~strike~~ `inline`.", "soft", "hard  ", "break", "", "> Quote", "", "- bullet", "", "3. ordered", "", "---", "", "```ts", "fenced", "```", "", "    indented", "", "[web](" + webURL("https") + ") [mail](mailto:a@example.com) [plain](" + webURL("http") + ")"].join("\n");
    const rendered = tree(body); const all = nodes(rendered);
    for (const tag of ["h1", "h2", "h3", "h4", "h5", "h6", "p", "em", "strong", "s", "code", "br", "blockquote", "ul", "ol", "li", "hr", "pre", "a"]) expect(all.some(n => n.type === tag), tag).toBe(true);
    expect(all.find(n => n.type === "ol")?.props).toMatchObject({ start: 3 });
    expect(all.filter(n => n.type === "pre").map(n => text(n))).toEqual(["fenced\n", "indented\n"]);
    expect(all.filter(n => n.type === "a")).toHaveLength(3);
    for (const n of all.filter(n => n.type === "a")) expect(n.props).toMatchObject({ rel: "noopener noreferrer" });
    expect(text(rendered)).toContain("Paragraph em strong strike inline.\nsoft\nhardbreak");
  });
  it("keeps unsafe markup, schemes and image input inert", () => {
    const image = webURL("https", "a.png");
    const body = '<script>alert(1)</script>\n<img src=x onerror=alert(1)>\n[x](javascript:alert(1))\n[d](data:text/plain,hello)\n![a](' + image + ')\n[local](/path)\n[peer](//example.com)\n[x](http:/path)\n[x](https:path)';
    const rendered = tree(body); const all = nodes(rendered);
    for (const n of all) {
      expect(["script", "img", "iframe"]).not.toContain(n.type);
      expect(Object.keys(n.props).some(k => /^on/i.test(k))).toBe(false);
      if (n.type === "a") expect(allowedLink((n.props as { href?: string }).href ?? "")).toBe(true);
    }
    expect(all.filter(n => n.type === "a")).toHaveLength(0);
    expect(text(rendered)).toContain('<script>alert(1)</script>');
    expect(text(rendered)).toContain('<img src=x onerror=alert(1)>');
    expect(text(rendered)).toContain("a (" + image + ")");
    for (const href of ["javascript:alert(1)", "data:text/plain,x", "/path", "//example.com", "https:\nexample.com", "mailto:a\u0000@example.com", "http:/path", "https:path", "https:" + "/" + "/", "http:" + "/" + "/?query"]) expect(allowedLink(href)).toBe(false);
  });
  it("uses only classes for table alignment", () => {
    const all = nodes(tree("| L | R | C |\n| :--- | ---: | :---: |\n| a | b | c |"));
    for (const tag of ["table", "thead", "tbody", "tr", "th", "td"]) expect(all.some(n => n.type === tag)).toBe(true);
    expect(all.filter(n => n.type === "th").map(n => (n.props as { class?: string }).class)).toEqual(["align-left", "align-right", "align-center"]);
    expect(all.filter(n => n.type === "td").map(n => (n.props as { class?: string }).class)).toEqual(["align-left", "align-right", "align-center"]);
    expect(all.some(n => "style" in n.props)).toBe(false);
  });
  it("shows unknown token text and rejects an unsafe parsed link", () => {
    const parser = new MarkdownIt({ html: false, linkify: false });
    const tokens = parser.parseInline("[label](mailto:a@example.com)", {})[0]!.children!;
    tokens[0]!.attrSet("href", "data:text/plain,x");
    expect(nodes(tokenNodes(tokens)).some(n => n.type === "a")).toBe(false);
    const unknown = new MarkdownIt.Token("unknown", "iframe", 0); unknown.content = "unknown text";
    expect(tokenNodes([unknown as Token])).toEqual(["unknown text"]);
  });
});

it("rendering captured Markdown fetches nothing and creates no source attributes", () => {
  const fetch = vi.fn(() => { throw new Error("unexpected network request"); });
  const request = vi.fn(() => { throw new Error("unexpected network request"); });
  vi.stubGlobal("fetch", fetch); vi.stubGlobal("XMLHttpRequest", request);
  try {
    const rendered = tree("# Document\n\n![image](" + webURL("https", "a.png") + ")\n\n[external](" + webURL("https") + ")\n\n<img src=x>");
    expect(fetch).not.toHaveBeenCalled(); expect(request).not.toHaveBeenCalled();
    for (const node of nodes(rendered)) expect(Object.keys(node.props).some(key => /^(src|srcset)$/i.test(key))).toBe(false);
  } finally { vi.unstubAllGlobals(); }
});

it.each([
  ["60,000 short lines", "a b c d e f g h\n".repeat(60000)],
  ["8,000 nested emphasis", "*a ".repeat(8000) + "a* ".repeat(8000)],
  ["60,000 emphasis pairs", "*a* ".repeat(60000)],
])("large document remains renderable: %s", (_name, body) => {
  const complete = body + "\n\nDOCUMENT-END";
  let rendered: ComponentChildren;
  expect(() => { rendered = Markdown({ body: complete }); }).not.toThrow();
  expect(text(rendered)).toContain("DOCUMENT-END");
  expect(text(rendered).length).toBeGreaterThan(body.length / 3);
});

it("keeps complete plain text when parsing or node construction fails", () => {
  const body = "# Entire body\n\nPreserve <script>text</script>.";
  const parse = vi.spyOn(MarkdownIt.prototype, "parse");
  try {
    parse.mockImplementationOnce(() => { throw new Error("parse failure"); });
    let fallback = Markdown({ body });
    expect(nodes(fallback).some(n => n.type === "pre" && (n.props as { class?: string }).class === "document-text")).toBe(true);
    expect(text(fallback)).toBe("Shown as plain text: this document could not be rendered." + body);
    const opening = new MarkdownIt.Token("link_open", "a", 1);
    opening.attrGet = () => { throw new Error("node failure"); };
    parse.mockReturnValueOnce([opening, new MarkdownIt.Token("link_close", "a", -1)]);
    fallback = Markdown({ body });
    expect(text(fallback)).toBe("Shown as plain text: this document could not be rendered." + body);
  } finally { parse.mockRestore(); }
});
