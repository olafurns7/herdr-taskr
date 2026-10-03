import { Component, type ComponentChildren } from "preact";

export function PlainDocument({ body }: { body: string }) {
  return <><p class="document-miss">Shown as plain text: this document could not be rendered.</p><pre class="document-text">{body}</pre></>;
}

// Parsing/node construction is guarded in Markdown; this boundary also catches
// Preact's own recursive diff and always retains the complete captured text.
export class MarkdownBoundary extends Component<{ body: string; children?: ComponentChildren }, { failed: boolean }> {
  override state = { failed: false };
  static override getDerivedStateFromError() { return { failed: true }; }
  override render() { return this.state.failed ? PlainDocument({ body: this.props.body }) : this.props.children; }
}
