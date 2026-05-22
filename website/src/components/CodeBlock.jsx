import React, { useState } from "react";

// Renders a terminal-flavoured code block. Lines starting "$ " are treated
// as prompts; lines whose trimmed text starts with "#" are dimmed comments.
// Anything else is rendered as program output.
export function CodeBlock({ children, lang = "sh", copyable = true, chrome = true, caption }) {
  const [copied, setCopied] = useState(false);
  const text = typeof children === "string"
    ? children
    : React.Children.toArray(children).join("");

  const onCopy = async () => {
    try {
      await navigator.clipboard?.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1400);
    } catch {
      // clipboard write can fail under file:// or sandboxed iframes; no-op.
    }
  };

  const lines = text.replace(/\n$/, "").split("\n");

  return (
    <div className="codeblock">
      {chrome && (
        <div className="codeblock-chrome">
          <span className="codeblock-lang mono">{lang}</span>
          {copyable && (
            <button className="codeblock-copy mono" onClick={onCopy} type="button">
              {copied ? "copied" : "copy"}
            </button>
          )}
        </div>
      )}
      <pre className="codeblock-body mono"><code>
        {lines.map((ln, i) => {
          let cls = "code-line";
          let content = ln;
          if (ln.startsWith("$ ")) {
            cls += " code-line-cmd";
            content = (
              <>
                <span className="code-prompt">$</span>
                <span className="code-rest"> {ln.slice(2)}</span>
              </>
            );
          } else if (ln.trim().startsWith("#")) {
            cls += " code-line-comment";
          } else if (ln.length === 0) {
            cls += " code-line-blank";
            content = " ";
          } else {
            cls += " code-line-out";
          }
          return <div key={i} className={cls}>{content}</div>;
        })}
      </code></pre>
      {caption && <div className="codeblock-caption mono">{caption}</div>}
    </div>
  );
}
