/* ページ上の要素をマウスで選ぶためのオーバーレイ。
   クリックされた要素の情報を window.__zvPick に置いて終了する。
   Esc で中止。ブラウザ側から一度だけ評価される想定。 */
(function () {
  if (window.__zvPickActive) { return "active"; }
  window.__zvPickActive = true;
  window.__zvPick = null;

  var box = document.createElement("div");
  box.style.cssText =
    "position:fixed;z-index:2147483647;pointer-events:none;border:2px solid #4aa3ff;" +
    "background:rgba(74,163,255,0.18);border-radius:2px;left:0;top:0;width:0;height:0";
  var tag = document.createElement("div");
  tag.style.cssText =
    "position:fixed;z-index:2147483647;pointer-events:none;background:#101418;color:#fff;" +
    "font:12px/1.4 ui-monospace,monospace;padding:2px 6px;border-radius:3px;left:0;top:0";
  document.documentElement.appendChild(box);
  document.documentElement.appendChild(tag);

  var current = null;

  function selectorOf(el) {
    var parts = [];
    var node = el;
    while (node && node.nodeType === 1 && parts.length < 6) {
      var part = node.nodeName.toLowerCase();
      if (node.id) { parts.unshift(part + "#" + node.id); break; }
      if (node.classList && node.classList.length) {
        part += "." + Array.prototype.slice.call(node.classList, 0, 3).join(".");
      }
      var parent = node.parentElement;
      if (parent) {
        var same = Array.prototype.filter.call(parent.children, function (c) {
          return c.nodeName === node.nodeName;
        });
        if (same.length > 1) { part += ":nth-of-type(" + (same.indexOf(node) + 1) + ")"; }
      }
      parts.unshift(part);
      node = parent;
    }
    return parts.join(" > ");
  }

  function onMove(ev) {
    var el = ev.target;
    if (!el || el === box || el === tag) { return; }
    current = el;
    var r = el.getBoundingClientRect();
    box.style.left = r.left + "px";
    box.style.top = r.top + "px";
    box.style.width = r.width + "px";
    box.style.height = r.height + "px";
    tag.textContent =
      el.nodeName.toLowerCase() + "  " + Math.round(r.width) + "x" + Math.round(r.height);
    tag.style.left = r.left + "px";
    tag.style.top = Math.max(0, r.top - 22) + "px";
  }

  function teardown() {
    document.removeEventListener("mousemove", onMove, true);
    document.removeEventListener("click", onClick, true);
    document.removeEventListener("keydown", onKey, true);
    if (box.parentNode) { box.parentNode.removeChild(box); }
    if (tag.parentNode) { tag.parentNode.removeChild(tag); }
    window.__zvPickActive = false;
  }

  function onKey(ev) {
    if (ev.key === "Escape") {
      ev.preventDefault();
      window.__zvPick = { cancelled: true };
      teardown();
    }
  }

  var WANTED = [
    "display", "position", "width", "height", "margin", "padding",
    "font-family", "font-size", "font-weight", "line-height", "letter-spacing",
    "color", "background-color", "background-image", "border", "border-radius",
    "box-shadow", "flex-direction", "justify-content", "align-items", "gap",
    "grid-template-columns", "z-index", "opacity", "overflow", "text-align"
  ];

  function onClick(ev) {
    ev.preventDefault();
    ev.stopPropagation();
    var el = current || ev.target;
    var r = el.getBoundingClientRect();
    var cs = window.getComputedStyle(el);
    var css = {};
    WANTED.forEach(function (k) {
      var v = cs.getPropertyValue(k);
      if (v && v !== "none" && v !== "normal" && v !== "auto" && v !== "0px") { css[k] = v; }
    });
    var html = el.outerHTML || "";
    var truncated = false;
    if (html.length > 4000) { html = html.slice(0, 4000); truncated = true; }
    window.__zvPick = {
      url: location.href,
      pageTitle: document.title,
      selector: selectorOf(el),
      tag: el.nodeName.toLowerCase(),
      rect: {
        x: Math.round(r.left), y: Math.round(r.top),
        w: Math.round(r.width), h: Math.round(r.height)
      },
      css: css,
      html: html,
      truncated: truncated,
      text: (el.innerText || "").slice(0, 600)
    };
    teardown();
  }

  document.addEventListener("mousemove", onMove, true);
  document.addEventListener("click", onClick, true);
  document.addEventListener("keydown", onKey, true);
  return "ok";
})();
