import { useMemo, useState } from "react";

interface Props {
  value: string;
  onChange: (value: string) => void;
  /** Names to suggest (e.g. existing variable names). */
  suggestions: string[];
  ariaLabel: string;
  placeholder?: string;
}

const MAX = 8;

/** A text input with a filtered suggestion dropdown. Free typing is always
 *  allowed (for new names); a suggestion is only auto-applied on Enter after
 *  the user arrows into it, or on click. */
export function SuggestInput({ value, onChange, suggestions, ariaLabel, placeholder }: Props) {
  const [open, setOpen] = useState(false);
  const [sel, setSel] = useState({ key: "", index: 0, touched: false });

  const matches = useMemo(() => {
    const q = value.trim().toLowerCase();
    const out: string[] = [];
    for (const s of suggestions) {
      if (s === value) continue;
      if (q === "" || s.toLowerCase().includes(q)) out.push(s);
      if (out.length >= MAX) break;
    }
    return out;
  }, [suggestions, value]);

  // Highlight resets when the query changes (no effect needed).
  const active = sel.key === value ? sel.index : 0;
  const touched = sel.key === value && sel.touched;
  const move = (index: number) => setSel({ key: value, index, touched: true });
  const show = open && matches.length > 0;

  const apply = (s: string) => {
    onChange(s);
    setOpen(false);
  };

  return (
    <div className="fb-expr-input">
      <input
        type="text"
        aria-label={ariaLabel}
        value={value}
        placeholder={placeholder}
        spellCheck={false}
        autoComplete="off"
        onChange={(e) => {
          onChange(e.target.value);
          setOpen(true);
        }}
        onFocus={() => setOpen(true)}
        onBlur={() => window.setTimeout(() => setOpen(false), 150)}
        onKeyDown={(e) => {
          if (!show) return;
          if (e.key === "ArrowDown") {
            e.preventDefault();
            move((active + 1) % matches.length);
          } else if (e.key === "ArrowUp") {
            e.preventDefault();
            move((active - 1 + matches.length) % matches.length);
          } else if (e.key === "Enter" && touched) {
            e.preventDefault();
            apply(matches[active]);
          } else if (e.key === "Escape") {
            e.preventDefault();
            setOpen(false);
          }
        }}
      />
      {show ? (
        <ul className="fb-expr-suggestions">
          {matches.map((s, i) => (
            <li key={s}>
              <button
                type="button"
                className={i === active ? "active" : ""}
                onMouseDown={(e) => {
                  e.preventDefault();
                  apply(s);
                }}
                onMouseEnter={() => move(i)}
              >
                <span className="fb-expr-sugg-label">{s}</span>
              </button>
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}
