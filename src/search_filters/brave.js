// Run at context creation, before Brave's own scripts initialize metrics.
if (localStorage.getItem('app.usageMetricsOptOut') === null) {
  localStorage.setItem('app.usageMetricsOptOut', 'true');
}

// Apply chrome/scrollbar styling as soon as a document root exists, rather
// than waiting for the page and all of its subresources to finish loading.
(() => {
  const install = () => {
    const root = document.head || document.documentElement;
    if (!root) return false;
    if (!document.getElementById('omni-page-filter')) {
      const style = document.createElement('style');
      style.id = 'omni-page-filter';
      style.textContent = __OMNI_BRAVE_CSS__;
      root.appendChild(style);
    }
    return true;
  };
  if (!install()) {
    const observer = new MutationObserver(() => {
      if (install()) observer.disconnect();
    });
    observer.observe(document, { childList: true, subtree: true });
  }
})();

// Offscreen Chromium's clipboard never reaches the desktop. Hand whatever the page
// copies (its copy buttons, or Ctrl+C on a selection) to omni, which owns the real one.
(() => {
  const send = (text) => {
    if (typeof text === 'string' && text) console.info('omni-copy:' + JSON.stringify(text));
  };
  const clipboard = navigator.clipboard;
  if (clipboard) {
    clipboard.writeText = async (text) => send(String(text));
    clipboard.write = async (items) => {
      for (const item of items) {
        if (item.types.includes('text/plain')) {
          send(await (await item.getType('text/plain')).text());
          return;
        }
      }
    };
  }
  // Bubble phase: a page handler that replaced the copied text has already run.
  window.addEventListener('copy', (event) => {
    send(event.clipboardData?.getData('text/plain') || String(getSelection() || ''));
  });
})();

// This notification comes from the real document's renderer. Browser-side
// about:blank callbacks can arrive after a navigation has already been queued.
if (location.pathname === '/ask') {
  const ready = () => console.info('omni-search-page-ready');
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', ready, {once:true});
  } else ready();
}
