import { expect, it } from "vitest";
import type { VNode } from "preact";
import { MarkdownBoundary } from "./DocumentBody";

it("Preact's error boundary retains the entire captured body after a child-render failure", () => {
  const body = "# Complete captured text\n<script>literal</script>";
  const boundary = new MarkdownBoundary({ body, children: "rendered" });
  expect(boundary.render()).toBe("rendered");
  boundary.state = MarkdownBoundary.getDerivedStateFromError();
  const fallback = boundary.render() as VNode<{ children: VNode[] }>;
  expect(fallback.props.children[0]).toMatchObject({ type: "p", props: { children: "Shown as plain text: this document could not be rendered." } });
  expect(fallback.props.children[1]).toMatchObject({ type: "pre", props: { class: "document-text", children: body } });
});
