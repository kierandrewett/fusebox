import { useMemo, useRef, useState } from "react";
import { EXPR_FUNCTIONS, EXPR_KEYWORDS } from "./exprMeta";

interface Suggestion {
  /** Text shown in the dropdown. */
  label: string;
  /** Secondary text (signature / doc / source). */
  detail: string;
  /** Text written into the textarea, replacing the active token. */
  insert: string;
}

interface TokenContext {
  kind: "input" | "var" | "ident";
  /** Index in the value where the replacement starts. */
  start: number;
  /** The already-typed prefix to filter on. */
  prefix: string;
}

interface Props {
  value: string;
  onChange: (value: string) => void;
  ariaLabel: string;
  /** Output field names of the block wired to IN (e.g. ["body","status_code"]). */
  inputFields: string[];
  /** Names of the automation's variables (without the $). */
  variableNames: string[];
}

const MAX_SUGGESTIONS = 8;

// Find the token the caret is sitting at the end of, and classify it so we
// know which pool to suggest from.
function tokenContextAt(text: string, caret: number): TokenContext | null {
  const before = text.slice(0, caret);
  let m: RegExpMatchArray | null;
  if ((m = before.match(/input\.([A-Za-z_]\w*)?$/))) {
    const prefix = m[1] ?? "";
    return { kind: "input", prefix, start: caret - prefix.length };
  }
  if ((m = before.match(/\$([A-Za-z_]\w*)?$/))) {
    const prefix = m[1] ?? "";
    return { kind: "var", prefix, start: caret - prefix.length - 1 };
  }
  if ((m = before.match(/([A-Za-z_]\w*)$/))) {
    return { kind: "ident", prefix: m[1], start: caret - m[1].length };
  }
  return null;
}

function suggestionsFor(
  ctx: TokenContext,
  inputFields: string[],
  variableNames: string[],
): Suggestion[] {
  const pre = ctx.prefix.toLowerCase();
  const starts = (s: string) => s.toLowerCase().startsWith(pre);
  const out: Suggestion[] = [];

  if (ctx.kind === "input") {
    for (const f of inputFields) {
      if (starts(f)) out.push({ label: f, detail: "input field", insert: f });
    }
    return out;
  }
  if (ctx.kind === "var") {
    for (const v of variableNames) {
      if (starts(v)) out.push({ label: `$${v}`, detail: "variable", insert: `$${v}` });
    }
    return out;
  }
  // identifier: keywords (incl. bare "input") then functions
  for (const k of EXPR_KEYWORDS) {
    if (starts(k)) {
      out.push({ label: k, detail: "keyword", insert: k === "input" ? "input." : k });
    }
  }
  for (const f of EXPR_FUNCTIONS) {
    if (starts(f.name)) out.push({ label: f.name, detail: f.signature, insert: `${f.name}(` });
  }
  return out;
}

export function ExpressionInput({
  value,
  onChange,
  ariaLabel,
  inputFields,
  variableNames,
}: Props) {
  const taRef = useRef<HTMLTextAreaElement>(null);
  const [open, setOpen] = useState(false);
  const [caret, setCaret] = useState(0);

  const token = useMemo(() => tokenContextAt(value, caret), [value, caret]);
  const suggestions = useMemo(() => {
    if (!token) return [];
    return suggestionsFor(token, inputFields, variableNames).slice(0, MAX_SUGGESTIONS);
  }, [token, inputFields, variableNames]);

  // Highlighted index is derived: it's the user's chosen offset, but it
  // resets to the top whenever the token under the caret changes (so a new
  // suggestion list always starts highlighted at the best match). No effect.
  const tokenKey = token ? `${token.kind}:${token.start}:${token.prefix}` : "";
  const [activeSel, setActiveSel] = useState({ key: "", index: 0 });
  const active = activeSel.key === tokenKey ? activeSel.index : 0;
  const setActive = (index: number) => setActiveSel({ key: tokenKey, index });

  const apply = (s: Suggestion) => {
    const ta = taRef.current;
    if (!ta || !token) return;
    const newBefore = value.slice(0, token.start) + s.insert;
    const after = value.slice(ta.selectionStart);
    const next = newBefore + after;
    onChange(next);
    setOpen(false);
    const pos = newBefore.length;
    requestAnimationFrame(() => {
      ta.focus();
      ta.setSelectionRange(pos, pos);
      setCaret(pos);
    });
  };

  const syncCaret = () => {
    const ta = taRef.current;
    if (ta) setCaret(ta.selectionStart);
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (open && suggestions.length > 0) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setActive((active + 1) % suggestions.length);
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        setActive((active - 1 + suggestions.length) % suggestions.length);
        return;
      }
      if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        apply(suggestions[active]);
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        setOpen(false);
        return;
      }
    }
  };

  const showDropdown = open && suggestions.length > 0;

  return (
    <div className="fb-expr-input">
      <textarea
        ref={taRef}
        aria-label={ariaLabel}
        rows={2}
        value={value}
        spellCheck={false}
        placeholder={'jsonDecode(input.body).daily[0] · $count + 1'}
        onChange={(e) => {
          onChange(e.target.value);
          setCaret(e.target.selectionStart);
          setOpen(true);
        }}
        onKeyDown={onKeyDown}
        onKeyUp={syncCaret}
        onClick={() => {
          syncCaret();
          setOpen(true);
        }}
        onFocus={() => setOpen(true)}
        onBlur={() => {
          // Delay so a click on a suggestion registers before we close.
          window.setTimeout(() => setOpen(false), 150);
        }}
      />
      {showDropdown ? (
        <ul className="fb-expr-suggestions">
          {suggestions.map((s, i) => (
            <li key={s.label}>
              <button
                type="button"
                className={i === active ? "active" : ""}
                onMouseDown={(e) => {
                  // mousedown fires before blur — keeps focus handling sane.
                  e.preventDefault();
                  apply(s);
                }}
                onMouseEnter={() => setActive(i)}
              >
                <span className="fb-expr-sugg-label">{s.label}</span>
                <span className="fb-expr-sugg-detail">{s.detail}</span>
              </button>
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}
