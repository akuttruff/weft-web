// The app shell, so weft opens like an app and at once: answered from the
// cache, refreshed from the Mac behind it (the next open has the new copy).
// The vendored library never changes under its name, so it is never asked
// for again. Sessions and the lists are never cached.
'use strict';

const CACHE = 'weft-web-v4';
const SHELL = [
  '/', '/app.js', '/style.css', '/manifest.webmanifest', '/icon.svg',
  '/icon-180.png', '/icon-512.png',
  '/vendor/xterm.js', '/vendor/xterm.css', '/vendor/addon-fit.js',
];

self.addEventListener('install', (e) => {
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener('activate', (e) => {
  e.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener('fetch', (e) => {
  const url = new URL(e.request.url);
  if (e.request.method !== 'GET' || url.origin !== location.origin) return;
  if (url.pathname.startsWith('/api/') || url.pathname === '/ws') return;
  const key = url.pathname === '/index.html' ? '/' : url.pathname;
  e.respondWith(
    caches.open(CACHE).then(async (cache) => {
      const hit = await cache.match(key);
      const refresh = fetch(e.request)
        .then((res) => {
          if (res.ok) cache.put(key, res.clone());
          return res;
        })
        .catch(() => hit);
      if (hit && url.pathname.startsWith('/vendor/')) return hit;
      if (hit) {
        e.waitUntil(refresh);
        return hit;
      }
      return refresh;
    }),
  );
});
