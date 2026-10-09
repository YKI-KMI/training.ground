import { EditorState } from "@codemirror/state";
import { EditorView, keymap } from "@codemirror/view";
import { basicSetup } from "codemirror";
import { python } from "@codemirror/lang-python";
import { javascript } from "@codemirror/lang-javascript";

const theme = EditorView.theme({
  "&": { backgroundColor: "#101215", color: "#e8e9ec", fontSize: "14px", height: "100%" },
  ".cm-scroller": { fontFamily: "ui-monospace, SFMono-Regular, Consolas, monospace", lineHeight: "1.55" },
  ".cm-gutters": { backgroundColor: "#101215", color: "#4b5058", border: "none" },
  ".cm-activeLine": { backgroundColor: "#15181c" },
  ".cm-activeLineGutter": { backgroundColor: "#15181c", color: "#8b9099" },
  "&.cm-focused .cm-cursor": { borderLeftColor: "#6ee7a8" },
  ".cm-selectionBackground, &.cm-focused .cm-selectionBackground": { backgroundColor: "#2a3b31 !important" },
  ".cm-content": { caretColor: "#6ee7a8" },
});

export function createEditor(
  parent: HTMLElement,
  doc: string,
  language: string,
  onRun: () => void,
): EditorView {
  const lang = language === "python" ? python() : javascript();
  return new EditorView({
    parent,
    state: EditorState.create({
      doc,
      extensions: [
        basicSetup,
        lang,
        theme,
        keymap.of([{ key: "Mod-Enter", run: () => { onRun(); return true; } }]),
      ],
    }),
  });
}
