// soundya developer build: collects what went wrong on the page from the very
// start (it runs in <head>, before the page's own scripts), for feedback.js.
(function () {
  var list = (window.__sdErrors = []);
  function add(kind, text) {
    list.push({ t: new Date().toISOString().slice(11, 19), kind: kind, text: String(text).slice(0, 600) });
    if (list.length > 40) list.shift();
  }
  window.addEventListener('error', function (e) {
    if (e.target && e.target !== window && (e.target.src || e.target.href)) {
      add('ресурс', 'не загрузился ' + (e.target.src || e.target.href));
    } else {
      add('js', (e.message || e.error) + (e.filename ? ' @ ' + e.filename.split('/').pop() + ':' + e.lineno + ':' + e.colno : ''));
    }
  }, true);
  window.addEventListener('unhandledrejection', function (e) {
    var r = e.reason;
    add('promise', r && r.stack ? r.stack.split('\n').slice(0, 3).join(' | ') : r);
  });
  var cerr = console.error;
  console.error = function () {
    try { add('console', Array.prototype.map.call(arguments, String).join(' ')); } catch (x) {}
    return cerr.apply(console, arguments);
  };
  // Failed requests to the page's own API: what, how it answered, how long it took.
  var f = window.fetch;
  if (f) {
    window.fetch = function (url, opts) {
      var t0 = Date.now();
      var what = ((opts && opts.method) || 'GET') + ' ' + String(url && url.url || url);
      return f.apply(this, arguments).then(function (r) {
        if (!r.ok && what.indexOf('/api/dev/') < 0) add('http', what + ' → ' + r.status + ' за ' + (Date.now() - t0) + ' мс');
        return r;
      }, function (err) {
        add('http', what + ' → ' + err + ' за ' + (Date.now() - t0) + ' мс');
        throw err;
      });
    };
  }
})();
