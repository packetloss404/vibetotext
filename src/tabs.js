// ARIA tablist pattern (WAI-ARIA APG).
//
// Implements the keyboard model + roving tabindex for a `role="tablist"`
// container. Activates the focused tab on ArrowLeft/ArrowRight (wraps), Home,
// End. Click handlers still call `onActivate`.
//
// Usage:
//   setupTablist({
//     tablist: document.getElementById('tabs'),
//     onActivate: (tab) => { ... },  // called on click OR keyboard activation
//   });
function setupTablist({ tablist, onActivate }) {
  if (!tablist) return;
  const tabs = Array.from(tablist.querySelectorAll('[role="tab"]'));
  if (tabs.length === 0) return;

  function activate(tab) {
    if (!tab) return;
    tabs.forEach((t) => {
      const active = t === tab;
      t.classList.toggle('active', active);
      t.setAttribute('aria-selected', active ? 'true' : 'false');
      t.setAttribute('tabindex', active ? '0' : '-1');
    });
    onActivate(tab);
  }

  function indexOf(tab) {
    return tabs.indexOf(tab);
  }

  function focusTab(index) {
    const wrapped = ((index % tabs.length) + tabs.length) % tabs.length;
    const tab = tabs[wrapped];
    tab.focus();
    activate(tab);
  }

  // Initialize the roving tabindex from aria-selected so first paint is correct
  // even if the static HTML didn't set it (defensive).
  tabs.forEach((t) => {
    const selected = t.getAttribute('aria-selected') === 'true';
    t.setAttribute('tabindex', selected ? '0' : '-1');
  });

  tablist.addEventListener('click', (e) => {
    const tab = e.target.closest('[role="tab"]');
    if (!tab || !tablist.contains(tab)) return;
    activate(tab);
  });

  tablist.addEventListener('keydown', (e) => {
    const tab = e.target.closest('[role="tab"]');
    if (!tab || !tablist.contains(tab)) return;
    const i = indexOf(tab);
    switch (e.key) {
      case 'ArrowRight':
      case 'ArrowDown':
        e.preventDefault();
        focusTab(i + 1);
        break;
      case 'ArrowLeft':
      case 'ArrowUp':
        e.preventDefault();
        focusTab(i - 1);
        break;
      case 'Home':
        e.preventDefault();
        focusTab(0);
        break;
      case 'End':
        e.preventDefault();
        focusTab(tabs.length - 1);
        break;
      default:
        break;
    }
  });
}
