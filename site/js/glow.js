// The hero glow follows the headline's words, not its box (house.css, "One glow").
// Measures the h1's lines and the free space around it and sets --gx, --gw, --room, --room-x on it.
(function () {
  function measure() {
    document.querySelectorAll('.hero h1').forEach(function (h) {
      var hero = h.closest('.hero'); if (!hero) return;
      var r = document.createRange(); r.selectNodeContents(h);
      var rects = Array.prototype.slice.call(r.getClientRects()).filter(function (x) { return x.width > 1; });
      if (!rects.length) return;
      var box = h.getBoundingClientRect(), hb = hero.getBoundingClientRect();
      var left = Math.min.apply(null, rects.map(function (x) { return x.left; }));
      var right = Math.max.apply(null, rects.map(function (x) { return x.right; }));
      h.style.setProperty('--gx', (left - box.left) + 'px');
      h.style.setProperty('--gw', (right - left) + 'px');
      h.style.setProperty('--room', Math.max(0, box.top - hb.top - 4) + 'px');
      h.style.setProperty('--room-x', Math.max(0, Math.min(left, innerWidth - right)) + 'px');
    });
  }
  measure();
  if (document.fonts && document.fonts.ready) document.fonts.ready.then(measure);
  addEventListener('resize', measure);
})();
