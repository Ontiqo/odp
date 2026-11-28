// The pointer's spotlight (DESIGN_SYSTEM.md §2): cards, bento cells and focal boxes light up under the pointer.
// Sets --mx/--my (the pointer inside the element) for house.css to draw with. One listener for the page.
(function () {
  var q = '.cell,.card,.list-card,.dl-card';
  if (!window.matchMedia || !matchMedia('(hover:hover)').matches) return;
  document.addEventListener('pointermove', function (e) {
    var el = e.target && e.target.closest && e.target.closest(q);
    if (!el) return;
    var r = el.getBoundingClientRect();
    el.style.setProperty('--mx', (e.clientX - r.left) + 'px');
    el.style.setProperty('--my', (e.clientY - r.top) + 'px');
  }, { passive: true });
})();
