// crab PWA service worker: cache the shell, never the live channel.
const CACHE = 'crab-shell-v8-short-hold';
const SHELL = ['./', './manifest.webmanifest', './icon.svg'];

self.addEventListener('install', (e) => {
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener('activate', (e) => {
  e.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim())
  );
});

// Network-first for everything (live data must never be stale); the cache is
// only a boot fallback when the host is briefly unreachable.
self.addEventListener('fetch', (e) => {
  if (e.request.url.includes('/ws')) return; // never intercept the socket
  e.respondWith(
    fetch(e.request).catch(() => caches.match(e.request).then((r) => r || caches.match('./')))
  );
});
